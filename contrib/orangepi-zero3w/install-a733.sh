#!/bin/bash
# install-a733.sh — one-shot SentryUSB installer for the Orange Pi Zero 3W
# (Allwinner A733 / sun60iw2).
#
# WHY THIS EXISTS: the upstream `install-pi.sh` fetches the STOCK arm64 binary,
# which has none of the A733 fixes — so setup dies on the XFS / root-shrink
# wall this board hits (no XFS in the BSP kernel; root-shrink loops). This
# wrapper runs the normal install, then swaps in the patched fork binary
# (ext4/btrfs backing cascade, OTG otg_role=usb_device, WiFi SSID/bars/dBm
# fallback, board name) BEFORE you run setup, so setup completes.
#
# USAGE (on a freshly-flashed Orange Pi Zero 3W, as a sudo-capable user):
#
#     # A) binary from a fork release (set the URL):
#     A733_BINARY_URL=https://github.com/<you>/Sentry-USB-Rusty/releases/download/a733-v1/sentryusb-a733 \
#       bash install-a733.sh
#
#     # B) binary already next to this script (./sentryusb-a733-patched):
#     bash install-a733.sh
#
# After it finishes: open the web UI (http://<pi-ip>/) and run the setup
# wizard. For an external SSD (recommended over SD — SD cards die under
# constant dashcam writes), plug the T5 into the MIDDLE Type-C (USB 3.0 host),
# leave the Tesla cable on the CORNER Type-C, and set DATA_DRIVE=/dev/sda in
# the setup so SentryUSB uses the SSD and skips root-shrink entirely.
set -uo pipefail

UPSTREAM_INSTALL_URL="${UPSTREAM_INSTALL_URL:-https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh}"
A733_BINARY_URL="${A733_BINARY_URL:-}"
TARGET=/opt/sentryusb/sentryusb-linux-arm64-a76
STASH=/opt/sentryusb/sentryusb-a733-patched
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_BINARY="${SELF_DIR}/sentryusb-a733-patched"

say() { echo "[install-a733] $*"; }
die() { echo "[install-a733] ERROR: $*" >&2; exit 1; }

SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

# 0. Sanity-check the board so we don't swap a binary onto the wrong SoC.
BOARD="$(cat /etc/orangepi-release 2>/dev/null | grep -iE 'board|zero ?3w' || true)$(tr -d '\0' </proc/device-tree/model 2>/dev/null)"
if ! echo "$BOARD" | grep -qiE 'zero ?3w|a733|sun60iw2'; then
  say "WARNING: this doesn't look like an Orange Pi Zero 3W (A733)."
  say "Detected: ${BOARD:-unknown}"
  say "The patched binary is A733-specific. Ctrl-C now if this is another board."
  sleep 5
fi

# 1. Source the patched binary BEFORE touching the system, so we fail early.
TMPBIN="$(mktemp)"
if [ -n "$A733_BINARY_URL" ]; then
  say "downloading patched binary from $A733_BINARY_URL"
  curl -fL "$A733_BINARY_URL" -o "$TMPBIN" || die "download failed"
elif [ -f "$LOCAL_BINARY" ]; then
  say "using local patched binary: $LOCAL_BINARY"
  cp "$LOCAL_BINARY" "$TMPBIN"
else
  rm -f "$TMPBIN"
  die "no patched binary — set A733_BINARY_URL or place sentryusb-a733-patched next to this script"
fi
# Cheap integrity check: an arm64 ELF starts with 0x7f 'E' 'L' 'F'.
head -c 4 "$TMPBIN" | grep -q $'\x7fELF' || { rm -f "$TMPBIN"; die "downloaded file is not an ELF binary"; }
chmod +x "$TMPBIN"

# 2. Run the upstream installer (installs the service + stock binary + scripts).
say "running upstream SentryUSB installer…"
curl -fsSL "$UPSTREAM_INSTALL_URL" | bash || die "upstream install-pi.sh failed"

[ -d /opt/sentryusb ] || die "upstream install did not create /opt/sentryusb"

# 3. Swap in the patched binary + keep a stash for repatch-a733.sh after OTAs.
say "installing patched A733 binary over the stock one"
$SUDO systemctl stop sentryusb 2>/dev/null || true
sleep 1
$SUDO cp "$TMPBIN" "$TARGET" && $SUDO chmod +x "$TARGET"
$SUDO cp "$TMPBIN" "$STASH" && $SUDO chmod +x "$STASH"
rm -f "$TMPBIN"

# 4. Install the post-OTA repatch helper alongside it (best-effort).
if [ -f "${SELF_DIR}/repatch-a733.sh" ]; then
  $SUDO cp "${SELF_DIR}/repatch-a733.sh" /opt/sentryusb/repatch-a733.sh
  $SUDO chmod +x /opt/sentryusb/repatch-a733.sh
  say "installed /opt/sentryusb/repatch-a733.sh (run after any future OTA)"
fi

# 5. Start and report.
$SUDO systemctl start sentryusb
sleep 4
if $SUDO systemctl is-active --quiet sentryusb; then
  IP="$(hostname -I 2>/dev/null | awk '{print $1}')"
  say "DONE — service active. Open http://${IP:-<pi-ip>}/ and run the setup wizard."
  say "SSD users: T5 → middle Type-C, Tesla → corner Type-C, set DATA_DRIVE=/dev/sda."
else
  die "service not active — check: journalctl -u sentryusb -n 30"
fi
