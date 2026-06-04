#!/usr/bin/env bash
#
# uninstall-service.sh — Remove the fm-ntrip systemd service from the Pi.
#
# Stops and disables the service and removes the unit and binary. The
# credentials file is left in place by default (it holds your password); pass
# --purge to remove it and /etc/fm-ntrip too.
#
# Usage: ./scripts/uninstall-service.sh [--purge]
#
set -euo pipefail

BIN_DEST=/usr/local/bin/fm-ntrip
UNIT_DEST=/etc/systemd/system/fm-ntrip.service
ENV_DIR=/etc/fm-ntrip
ENV_DEST="${ENV_DIR}/fm-ntrip.env"
LOGROTATE_DEST=/etc/logrotate.d/fm-ntrip
LOG_FILE=/var/log/fm-ntrip.log

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] && die "Do not run as root. Run as your normal user; the script calls sudo where needed."

PURGE=0
[[ "${1:-}" == "--purge" ]] && PURGE=1

# Stop/disable only if the unit is known to systemd.
if systemctl list-unit-files fm-ntrip.service >/dev/null 2>&1 \
   && systemctl cat fm-ntrip.service >/dev/null 2>&1; then
    log "Stopping and disabling fm-ntrip.service"
    sudo systemctl disable --now fm-ntrip.service || true
else
    log "fm-ntrip.service not registered — nothing to stop"
fi

if [[ -f "$UNIT_DEST" ]]; then
    log "Removing unit ${UNIT_DEST}"
    sudo rm -f "$UNIT_DEST"
fi

if [[ -f "$LOGROTATE_DEST" ]]; then
    log "Removing logrotate rule ${LOGROTATE_DEST}"
    sudo rm -f "$LOGROTATE_DEST"
fi

log "Reloading systemd"
sudo systemctl daemon-reload
sudo systemctl reset-failed fm-ntrip.service 2>/dev/null || true

if [[ -e "$BIN_DEST" ]]; then
    log "Removing binary ${BIN_DEST}"
    sudo rm -f "$BIN_DEST"
fi

if [[ $PURGE -eq 1 ]]; then
    if [[ -e "$ENV_DEST" || -d "$ENV_DIR" ]]; then
        log "Purging credentials ${ENV_DIR}"
        sudo rm -rf "$ENV_DIR"
    fi
    if [[ -e "$LOG_FILE" ]]; then
        log "Purging log file ${LOG_FILE}*"
        sudo rm -f "${LOG_FILE}" "${LOG_FILE}".*
    fi
else
    [[ -f "$ENV_DEST" ]] && warn "Left credentials in place: ${ENV_DEST} (use --purge to remove)"
    [[ -f "$LOG_FILE" ]] && warn "Left logs in place: ${LOG_FILE} (use --purge to remove)"
fi

log "Done. fm-ntrip service removed."
