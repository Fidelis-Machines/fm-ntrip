#!/usr/bin/env bash
#
# install-service.sh — Install fm-ntrip as a systemd service on the Raspberry Pi.
#
# Self-contained install under /opt/fm-ntrip:
#   /opt/fm-ntrip/bin       fm-ntrip + fm-ntrip-client binaries
#   /opt/fm-ntrip/scripts   these helper scripts
#   /opt/fm-ntrip/etc       fm-ntrip.env credentials
# The systemd unit (/etc/systemd/system) and logrotate rule
# (/etc/logrotate.d) live in their required system dirs but point at /opt.
# The service User= is set to the invoking user (so it can open the serial
# device via the dialout group). Builds the release binaries first if missing.
#
# Usage: ./scripts/install-service.sh [REPO_DIR]
#
#   REPO_DIR        Path to the fm-ntrip checkout (auto-detected if omitted).
#   SERVICE_USER    Override the account the service runs as (default: caller).
#   ENABLE_NOW=0    Install only; do not enable/start the service.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PREFIX=/opt/fm-ntrip
BIN_DIR="${PREFIX}/bin"
SCRIPTS_DIR="${PREFIX}/scripts"
ETC_DIR="${PREFIX}/etc"
ENV_DEST="${ETC_DIR}/fm-ntrip.env"
UNIT_DEST=/etc/systemd/system/fm-ntrip.service
LOGROTATE_DEST=/etc/logrotate.d/fm-ntrip

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] && die "Do not run as root. Run as your normal user; the script calls sudo where needed."

# The account the service should run as — the human user who owns the install,
# not root. Honour an explicit override, else the sudo caller, else $USER.
SERVICE_USER="${SERVICE_USER:-${SUDO_USER:-$USER}}"
id "$SERVICE_USER" >/dev/null 2>&1 || die "service user '$SERVICE_USER' does not exist"

# True if the given dir holds the fm-ntrip Cargo manifest.
is_repo() { [[ -f "$1/Cargo.toml" ]] && grep -q '^name = "fm-ntrip"' "$1/Cargo.toml" 2>/dev/null; }

find_repo() {
    local candidates=(
        "${FM_NTRIP_DIR:-}" "${1:-}"
        "${SCRIPT_DIR}/.." "${SCRIPT_DIR}"
        "${PWD}" "${PWD}/fm-ntrip" "${HOME}/fm-ntrip"
        "${PWD}/fm-trip" "${HOME}/fm-trip" # legacy name, before the fm-ntrip rename
    )
    local c
    for c in "${candidates[@]}"; do
        [[ -n "$c" && -d "$c" ]] || continue
        if is_repo "$c"; then ( cd "$c" && pwd ); return 0; fi
    done
    return 1
}

REPO_DIR="$(find_repo "${1:-}")" || die "Could not find the fm-ntrip checkout. Pass its path: $0 /path/to/fm-ntrip"
UNIT_SRC="${REPO_DIR}/systemd/fm-ntrip.service"
ENV_SRC="${REPO_DIR}/systemd/fm-ntrip.env.example"
LOGROTATE_SRC="${REPO_DIR}/systemd/fm-ntrip.logrotate"
[[ -f "$UNIT_SRC" ]] || die "unit template not found at $UNIT_SRC"
[[ -f "$ENV_SRC"  ]] || die "env template not found at $ENV_SRC"

# --- Binaries ---------------------------------------------------------------
SERVER_SRC="${REPO_DIR}/target/release/fm-ntrip"
CLIENT_SRC="${REPO_DIR}/target/release/fm-ntrip-client"
if [[ ! -x "$SERVER_SRC" || ! -x "$CLIENT_SRC" ]]; then
    log "Release binaries not built yet — building (this can take several minutes)"
    command -v cargo >/dev/null 2>&1 || { [[ -f "${HOME}/.cargo/env" ]] && . "${HOME}/.cargo/env"; }
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Run scripts/install-devel.sh first to install Rust."
    cargo build --release --manifest-path "${REPO_DIR}/Cargo.toml"
fi
[[ -x "$SERVER_SRC" ]] || die "server binary still missing at $SERVER_SRC"
[[ -x "$CLIENT_SRC" ]] || die "client binary still missing at $CLIENT_SRC"

log "Installing binaries → ${BIN_DIR}"
sudo install -d -m 0755 "$BIN_DIR"
sudo install -m 0755 "$SERVER_SRC" "${BIN_DIR}/fm-ntrip"
sudo install -m 0755 "$CLIENT_SRC" "${BIN_DIR}/fm-ntrip-client"

# --- Scripts ----------------------------------------------------------------
log "Installing scripts → ${SCRIPTS_DIR}"
sudo install -d -m 0755 "$SCRIPTS_DIR"
sudo install -m 0755 "${REPO_DIR}/scripts/"*.sh "$SCRIPTS_DIR"/

# --- Credentials file -------------------------------------------------------
sudo install -d -m 0755 "$ETC_DIR"
if [[ -f "$ENV_DEST" ]]; then
    log "Credentials file already exists → ${ENV_DEST} (left unchanged)"
else
    log "Installing credentials template → ${ENV_DEST}"
    sudo install -m 0600 "$ENV_SRC" "$ENV_DEST"
    warn "Edit ${ENV_DEST} and set a real NTRIP_PASS before exposing the caster."
fi

# --- Unit -------------------------------------------------------------------
# Substitute the User= line with the chosen service account.
log "Installing unit → ${UNIT_DEST} (User=${SERVICE_USER})"
tmp_unit="$(mktemp)"
trap 'rm -f "$tmp_unit"' EXIT
sed "s/^User=.*/User=${SERVICE_USER}/" "$UNIT_SRC" > "$tmp_unit"
sudo install -m 0644 "$tmp_unit" "$UNIT_DEST"

# --- Log rotation -----------------------------------------------------------
if [[ -f "$LOGROTATE_SRC" ]]; then
    log "Installing logrotate rule → ${LOGROTATE_DEST}"
    sudo install -m 0644 "$LOGROTATE_SRC" "$LOGROTATE_DEST"
else
    warn "logrotate template not found at $LOGROTATE_SRC — skipping (log file will grow unbounded)"
fi

# --- Serial-port access -----------------------------------------------------
if id -nG "$SERVICE_USER" | tr ' ' '\n' | grep -qx dialout; then
    log "User '$SERVICE_USER' is in the 'dialout' group (serial access OK)"
else
    warn "User '$SERVICE_USER' is NOT in 'dialout' — adding it (needed to open /dev/ttyACM0)"
    sudo usermod -aG dialout "$SERVICE_USER"
fi

# --- Enable -----------------------------------------------------------------
log "Reloading systemd"
sudo systemctl daemon-reload

if [[ "${ENABLE_NOW:-1}" == "1" ]]; then
    log "Enabling and starting fm-ntrip"
    sudo systemctl enable --now fm-ntrip.service
    echo
    sudo systemctl --no-pager --full status fm-ntrip.service || true
else
    log "Skipping enable/start (ENABLE_NOW=0). Start it with: sudo systemctl enable --now fm-ntrip"
fi

echo
log "Done. Installed under ${PREFIX}"
echo "  Server:  ${BIN_DIR}/fm-ntrip"
echo "  Client:  ${BIN_DIR}/fm-ntrip-client"
echo "  Logs:    tail -f /var/log/fm-ntrip.log  (rotated by ${LOGROTATE_DEST})"
echo "  Status:  systemctl status fm-ntrip"
echo "  Config:  ${ENV_DEST}  and  ${UNIT_DEST}"
