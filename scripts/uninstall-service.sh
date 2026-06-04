#!/usr/bin/env bash
#
# uninstall-service.sh — Remove the fm-ntrip systemd service from the Pi.
#
# Stops and disables the service, removes the unit, logrotate rule, and the
# /opt/fm-ntrip binaries + scripts. The credentials directory (/opt/fm-ntrip/etc)
# is left in place by default (it holds your password); pass --purge to remove
# the whole /opt/fm-ntrip tree and the log file too.
#
# Usage: ./scripts/uninstall-service.sh [--purge]
#
set -euo pipefail

PREFIX=/opt/fm-ntrip
ETC_DIR="${PREFIX}/etc"
UNIT_DEST=/etc/systemd/system/fm-ntrip.service
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

if [[ $PURGE -eq 1 ]]; then
    if [[ -d "$PREFIX" ]]; then
        log "Purging install tree ${PREFIX}"
        sudo rm -rf "$PREFIX"
    fi
    if [[ -e "$LOG_FILE" ]]; then
        log "Purging log file ${LOG_FILE}*"
        sudo rm -f "${LOG_FILE}" "${LOG_FILE}".*
    fi
else
    # Remove binaries and scripts, but preserve the credentials in etc/.
    log "Removing ${PREFIX}/bin and ${PREFIX}/scripts"
    sudo rm -rf "${PREFIX}/bin" "${PREFIX}/scripts"
    [[ -d "$ETC_DIR" ]] && warn "Left credentials in place: ${ETC_DIR} (use --purge to remove)"
    [[ -f "$LOG_FILE" ]] && warn "Left logs in place: ${LOG_FILE} (use --purge to remove)"
fi

log "Done. fm-ntrip service removed."
