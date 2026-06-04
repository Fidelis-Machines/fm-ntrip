#!/usr/bin/env bash
#
# install-devel.sh — Install Rust and build fm-ntrip on a Raspberry Pi.
#
# Installs the Rust toolchain via rustup, the system build prerequisites,
# grants the current user serial-port access, and builds the release binary.
#
# Usage: ./scripts/install-devel.sh [REPO_DIR]
#
#   REPO_DIR  Path to the fm-ntrip checkout (the dir containing Cargo.toml).
#             May also be given via the FM_NTRIP_DIR environment variable.
#             If omitted, the script searches sensible locations.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }

# True if the given dir holds the fm-ntrip Cargo manifest.
is_repo() { [[ -f "$1/Cargo.toml" ]] && grep -q '^name = "fm-ntrip"' "$1/Cargo.toml" 2>/dev/null; }

# Locate the fm-ntrip checkout. The script may be run from inside the repo
# (scripts/ subdir) or copied somewhere standalone, so check several places.
find_repo() {
    local candidates=(
        "${FM_NTRIP_DIR:-}"      # explicit env override
        "${1:-}"                 # explicit CLI arg
        "${SCRIPT_DIR}/.."       # script lives in <repo>/scripts/
        "${SCRIPT_DIR}"          # script lives at repo root
        "${PWD}"                 # invoked from inside the repo
        "${PWD}/fm-ntrip"        # repo cloned under cwd
        "${HOME}/fm-ntrip"
        "${PWD}/fm-trip"         # legacy name, before the fm-ntrip rename
        "${HOME}/fm-trip"
    )
    local c
    for c in "${candidates[@]}"; do
        [[ -n "$c" && -d "$c" ]] || continue
        if is_repo "$c"; then ( cd "$c" && pwd ); return 0; fi
    done
    return 1
}

REPO_DIR="$(find_repo "${1:-}")" || die "Could not find the fm-ntrip checkout. Run this from inside the repo, or pass its path: $0 /path/to/fm-ntrip"

[[ $EUID -eq 0 ]] && die "Do not run as root. Run as your normal user; the script calls sudo where needed."

# --- System prerequisites --------------------------------------------------
if command -v apt-get >/dev/null 2>&1; then
    log "Installing build prerequisites (build-essential, pkg-config, curl)"
    sudo apt-get update
    sudo apt-get install -y build-essential pkg-config curl
else
    warn "apt-get not found — install a C toolchain (build-essential equivalent) manually."
fi

# --- Rust toolchain ---------------------------------------------------------
if command -v rustc >/dev/null 2>&1; then
    log "Rust already installed: $(rustc --version)"
else
    log "Installing Rust via rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi

# Make cargo/rustc available in this shell whether freshly or previously installed.
# shellcheck disable=SC1090
[[ -f "${HOME}/.cargo/env" ]] && . "${HOME}/.cargo/env"

command -v cargo >/dev/null 2>&1 || die "cargo not on PATH after install. Open a new shell and re-run, or 'source \$HOME/.cargo/env'."

log "Toolchain ready: $(rustc --version), $(cargo --version)"

# --- Serial-port access -----------------------------------------------------
# The simpleRTK2B / ZED-F9P USB CDC device shows up as /dev/ttyACM0, owned by
# the 'dialout' group. Add the user so fm-ntrip can open it without root.
if getent group dialout >/dev/null 2>&1; then
    if id -nG "$USER" | tr ' ' '\n' | grep -qx dialout; then
        log "User '$USER' already in 'dialout' group"
    else
        log "Adding '$USER' to 'dialout' group for serial access"
        sudo usermod -aG dialout "$USER"
        warn "Log out and back in (or reboot) for the new group membership to take effect."
    fi
fi

# --- Build ------------------------------------------------------------------
log "Building fm-ntrip (release) — this can take several minutes on a Pi"
cargo build --release --manifest-path "${REPO_DIR}/Cargo.toml"

BIN="${REPO_DIR}/target/release/fm-ntrip"
[[ -x "$BIN" ]] || die "Build reported success but binary not found at $BIN"

log "Done. Binary: $BIN"
echo
echo "Try it:"
echo "  $BIN --help"
echo "  $BIN --device /dev/ttyACM0 --listen 0.0.0.0:2101 --mountpoint RTCM3"
