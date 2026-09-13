#!/usr/bin/env bash
#
# ph-reactor installer (curl | bash).
#
#   curl -fsSL https://<host>/ph-reactor/install.sh | bash
#
# Builds the ph-reactor daemon from source (a Rust/cargo build) and installs it
# for the current user:
#   - binary            -> ~/.local/bin/ph-reactor
#   - tray desktop file -> ~/.local/share/applications/ph-reactor.desktop
#   - autostart entry   -> ~/.config/autostart/ph-reactor.desktop
#
# It does NOT start the daemon for you; run `ph-reactor` to launch it (it
# appears in the status-bar tray). Use `ph-reactor drive add <url>` to sync a
# remote drive, and `ph-reactor status` to check it.
#
# Set PH_REACTOR_SRC to a local checkout to install from a specific tree
# (defaults to a shallow clone of the repo).
set -euo pipefail

log() { printf '\033[1;34m[ph-reactor]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[ph-reactor] error:\033[0m %s\n' "$*" >&2; exit 1; }

# --- Preconditions ---------------------------------------------------------
command -v cargo >/dev/null 2>&1 \
  || die "cargo is required (https://rustup.rs). Install Rust and re-run."
command -v rustc >/dev/null 2>&1 \
  || die "rustc is required (https://rustup.rs). Install Rust and re-run."

# --- Source tree -----------------------------------------------------------
SRC="${PH_REACTOR_SRC:-}"
if [[ -z "${SRC}" ]]; then
  log "Cloning the ph-reactor source..."
  SRC="$(mktemp -d /tmp/ph-reactor-src.XXXXXX)"
  git clone --depth 1 \
    "https://github.com/powerhouse-inc/ph-reactor-native.git" "${SRC}" \
    || die "git clone failed; set PH_REACTOR_SRC to a local checkout."
fi
[[ -f "${SRC}/Cargo.toml" ]] || die "no Cargo.toml in ${SRC}"

# --- Build -----------------------------------------------------------------
log "Building ph-reactor (release)..."
cargo build --release --locked --manifest-path "${SRC}/Cargo.toml"

BIN="$(cd "${SRC}" && pwd)/target/release/ph-reactor"
[[ -x "${BIN}" ]] || die "build did not produce ${BIN}"

# --- Install ---------------------------------------------------------------
LOCAL_BIN="${HOME}/.local/bin"
APPS_DIR="${HOME}/.local/share/applications"
AUTOSTART_DIR="${HOME}/.config/autostart"
mkdir -p "${LOCAL_BIN}" "${APPS_DIR}" "${AUTOSTART_DIR}"

log "Installing binary to ${LOCAL_BIN}/ph-reactor"
install -m755 "${BIN}" "${LOCAL_BIN}/ph-reactor"

DESKTOP="$(cd "${SRC}" && pwd)/packaging/ph-reactor.desktop"
[[ -f "${DESKTOP}" ]] || die "missing ${DESKTOP}"

log "Installing tray desktop file + autostart entry"
# For the user-session autostart, point Exec at the absolute installed binary.
sed "s|^Exec=.*|Exec=${LOCAL_BIN}/ph-reactor run|" "${DESKTOP}" \
  > "${AUTOSTART_DIR}/ph-reactor.desktop"
install -m644 "${DESKTOP}" "${APPS_DIR}/ph-reactor.desktop"

# --- Finish ----------------------------------------------------------------
log "Done."
cat <<'EOF'

  The reactor is installed. To start it (it appears in the status-bar tray):

      ph-reactor

  To sync a remote drive (for example a knowledge vault):

      ph-reactor drive add <drive-url>

  Other useful commands:
      ph-reactor status        # health, identity, peers, drives
      ph-reactor doctor        # diagnose connectivity / registry issues
      ph-reactor logs -f       # follow the daemon log
      ph-reactor stop          # stop the daemon

  On most desktops the autostart entry launches it at login. Make sure
  ~/.local/bin is on your PATH:

      echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
EOF
