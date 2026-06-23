#!/bin/bash
# repatch-a733.sh — re-apply the Orange Pi Zero 3W (Allwinner A733) fixes after
# a SentryUSB OTA / "Update" overwrites the stock binary.
#
# WHY: the upstream prebuilt binary has NONE of the A733 fixes (ext4/btrfs
# backing-fs cascade, OTG otg_role=usb_device, WiFi SSID/bars/dBm fallback,
# board name). An OTA replaces /opt/sentryusb/sentryusb-linux-arm64-a76 with
# that stock build, so WiFi/dBm break and a re-setup would hit the old XFS/
# shrink-loop wall. This restores the stashed patched binary + the shell fixes.
#
# It does NOT need the network or a rebuild — the patched binary is stashed
# right here on the Pi. Run as root after any update:
#
#     sudo /opt/sentryusb/repatch-a733.sh
#
# If upstream has advanced and you want the NEW upstream features TOO, you need
# a fresh cross-build from the chufleco fork on the workstation instead (this
# only restores the last-known-good patched build).
set -uo pipefail

PATCHED=/opt/sentryusb/sentryusb-a733-patched
TARGET=/opt/sentryusb/sentryusb-linux-arm64-a76
ARCHIVELOOP=/root/bin/archiveloop
# Auto-detect the active WiFi connection (override with the WIFI_CONN env var).
WIFI_CONN="${WIFI_CONN:-$(nmcli -t -f NAME,TYPE connection show --active 2>/dev/null \
  | awk -F: '$2=="802-11-wireless"{print $1; exit}')}"

if [ "$(id -u)" -ne 0 ]; then
  echo "[repatch] must run as root:  sudo $0" >&2
  exit 1
fi

echo "[repatch] SentryUSB A733 re-patch starting…"

# 1. Binary — restore the patched build over whatever the OTA left.
if [ ! -f "$PATCHED" ]; then
  echo "[repatch] ERROR: stashed patched binary missing at $PATCHED" >&2
  echo "[repatch] Rebuild + redeploy from the workstation fork instead." >&2
  exit 1
fi
if cmp -s "$PATCHED" "$TARGET"; then
  echo "[repatch] binary already patched (identical) — skipping swap"
else
  echo "[repatch] stopping service + restoring patched binary"
  systemctl stop sentryusb || true
  sleep 1
  cp "$PATCHED" "$TARGET"
  chmod +x "$TARGET"
  echo "[repatch] binary restored"
fi

# 2. archiveloop — the g_ether unload must not FATAL on the A733 (no g_ether
#    module). If an OTA shipped the stock archiveloop, re-apply the guard.
if [ -f "$ARCHIVELOOP" ]; then
  if grep -qE '^[[:space:]]*modprobe -r g_ether[[:space:]]*$' "$ARCHIVELOOP"; then
    echo "[repatch] re-applying archiveloop g_ether guard"
    sed -i -E 's@^([[:space:]]*)modprobe -r g_ether[[:space:]]*$@\1modprobe -r g_ether 2>/dev/null || true@' "$ARCHIVELOOP"
  else
    echo "[repatch] archiveloop g_ether guard already present — skipping"
  fi
fi

# 3. WiFi (optional) — restore the 5GHz band if an update reset the active
#    connection to 2.4GHz. Off by default (personal preference, not an A733 fix);
#    enable with FORCE_5GHZ=1.
if [ "${FORCE_5GHZ:-0}" = "1" ] && [ -n "$WIFI_CONN" ]; then
  CONN_FILE="/etc/NetworkManager/system-connections/${WIFI_CONN}.nmconnection"
  if [ -f "$CONN_FILE" ] && grep -q '^band=bg' "$CONN_FILE"; then
    echo "[repatch] restoring 5GHz band on $WIFI_CONN"
    sed -i 's/^band=bg/band=a/' "$CONN_FILE"
    nmcli connection reload 2>/dev/null || true
    nmcli connection up "$WIFI_CONN" 2>/dev/null || true
  fi
fi

# 4. Restart and report.
echo "[repatch] starting service"
systemctl restart sentryusb
sleep 4
if systemctl is-active --quiet sentryusb; then
  echo "[repatch] DONE — service active"
  DBM=$(curl -s http://localhost:80/api/status 2>/dev/null | grep -oE '"wifi_signal_dbm":[-0-9]+' || true)
  FREQ=$(curl -s http://localhost:80/api/status 2>/dev/null | grep -oE '"wifi_freq":"[0-9]+"' || true)
  echo "[repatch] verify: ${DBM:-no dBm} ${FREQ:-no freq}"
else
  echo "[repatch] WARNING: service not active — check: journalctl -u sentryusb -n 30" >&2
fi
