#!/bin/bash -eu
#
# SentryUSB (Rust) Installer
#
# Minimal installer — downloads the Rust binary and installs the systemd
# service. The binary itself handles ALL setup (partitioning, disk images,
# system config, etc.) via the web UI setup wizard.
#
# Usage:
#   sudo -i
#   curl -fsSL https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh | bash
#
# Or with a local binary:
#   bash install-pi.sh /path/to/sentryusb-binary

REPO="Sentry-Six/Sentry-USB-Rusty"
INSTALL_DIR="/opt/sentryusb"
BINARY_NAME="sentryusb"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC} $1"; }
ok()    { echo -e "${GREEN}[OK]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error_exit() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

if [[ $EUID -ne 0 ]]; then
    error_exit "This script must be run as root. Try: sudo -i"
fi

# Backward-compat: the Go install.sh accepted `norootshrink` as its
# first arg to skip the root-partition shrink step (used when an
# external USB/NVMe data drive supplies the storage). In the Rust
# port the shrink moved into the binary's setup wizard — it's
# automatically skipped when DATA_DRIVE is set on the Storage step
# (or in /root/sentryusb.conf). Recognize the legacy arg here so it
# doesn't silently look like a "local binary path" lookup, and
# clear it so it doesn't get treated as one.
case "${1:-}" in
    norootshrink|no-root-shrink|NOROOTSHRINK|norrotshrink)
        info "Note: '$1' was a Go-era install arg; in the Rust port,"
        info "  pick your external drive on the wizard's Storage step"
        info "  (sets DATA_DRIVE) to skip root-partition shrinking."
        shift || true
        ;;
esac

# ── Step 1: /sentryusb Symlink ─────────────────────────────────────

info "Setting up /sentryusb symlink..."
if [ ! -L /sentryusb ]; then
    rm -rf /sentryusb
    if [ -d /boot/firmware ] && findmnt --fstab /boot/firmware &> /dev/null; then
        ln -s /boot/firmware /sentryusb
    else
        ln -s /boot /sentryusb
    fi
fi
ok "/sentryusb -> $(readlink /sentryusb)"

# ── Step 2: Install SentryUSB Binary(es) + Picker ──────────────────
#
# On aarch64 we stage three per-CPU-tuned variants (a53/a72/a76) so each
# Pi runs code matched to its microarchitecture. The runtime picker
# (sentryusb-pick-binary, installed below) symlinks the best one to
# sentryusb-current at every service start.
#
# On armv7/armv6 there's no microarchitectural split — single variant.
# Same picker handles all four cases via /proc/cpuinfo detection.

mkdir -p "$INSTALL_DIR"

# Detect userspace arch first. The aarch64 case stages multiple binaries;
# the others stage one. Same detection logic the picker uses at boot,
# duplicated here only to decide which release files to download.
if command -v dpkg >/dev/null 2>&1; then
    DPKG_ARCH=$(dpkg --print-architecture)
    case "$DPKG_ARCH" in
        arm64)  ARCH_FAMILY="aarch64" ;;
        armhf)  ARCH_FAMILY="armv7" ;;
        armel)  ARCH_FAMILY="armv6" ;;
        amd64)  ARCH_FAMILY="amd64" ;;
        *)      error_exit "Unsupported userspace architecture: $DPKG_ARCH" ;;
    esac
else
    case "$(uname -m)" in
        aarch64) ARCH_FAMILY="aarch64" ;;
        armv7l)  ARCH_FAMILY="armv7" ;;
        armv6l)  ARCH_FAMILY="armv6" ;;
        x86_64)  ARCH_FAMILY="amd64" ;;
        *)       error_exit "Unsupported architecture: $(uname -m)" ;;
    esac
fi

# Map the family → suffixes we need to download. aarch64 expands to three.
case "$ARCH_FAMILY" in
    aarch64) SUFFIXES="linux-arm64-a53 linux-arm64-a72 linux-arm64-a76" ;;
    armv7)   SUFFIXES="linux-armv7" ;;
    armv6)   SUFFIXES="linux-armv6" ;;
    amd64)   SUFFIXES="linux-amd64" ;;
esac

if [ -n "${1:-}" ] && [ -f "${1:-}" ]; then
    # Local-binary mode — installer was invoked with a path to a binary on
    # disk. Skip downloads and stage that one binary under all matching
    # CPU suffixes so the picker always finds something. (This is a
    # convenience for local dev builds; production installs use the
    # download path below.)
    info "Installing binary from local path: $1"
    for sfx in $SUFFIXES; do
        cp "$1" "$INSTALL_DIR/$BINARY_NAME-$sfx"
        chmod +x "$INSTALL_DIR/$BINARY_NAME-$sfx"
    done
    ok "Local binary staged under $(echo $SUFFIXES | tr ' ' '\n' | wc -l) variant(s)"
else
    info "Downloading SentryUSB binary variants from GitHub..."

    for sfx in $SUFFIXES; do
        DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-${sfx}"
        TMP="/tmp/${BINARY_NAME}-${sfx}.new"
        success=false
        for attempt in $(seq 1 5); do
            if curl -fsSL "$DOWNLOAD_URL" -o "$TMP" 2>/dev/null; then
                chmod +x "$TMP"
                mv "$TMP" "$INSTALL_DIR/$BINARY_NAME-$sfx"
                ok "Downloaded $BINARY_NAME-$sfx"
                success=true
                break
            fi
            warn "Download of $sfx failed (attempt $attempt/5), retrying..."
            sleep 3
        done
        if [ "$success" != true ]; then
            error_exit "Failed to download $BINARY_NAME-$sfx after 5 attempts"
        fi
    done

    RELEASE_TAG=$(curl -fsSL --max-time 10 \
        "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep '"tag_name"' | head -1 \
        | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/' || true)
    if [ -n "${RELEASE_TAG:-}" ]; then
        echo "$RELEASE_TAG" > "$INSTALL_DIR/version"
        ok "Version: $RELEASE_TAG"
    fi
fi

# ── Picker script (selects the right binary at every service start) ──
PICKER_URL="https://raw.githubusercontent.com/${REPO}/main/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary"
PICKER_DST="/usr/local/bin/sentryusb-pick-binary"
PICKER_LOCAL_FALLBACK="$(dirname "${1:-/dev/null}")/sentryusb-pick-binary"
if [ -f "$PICKER_LOCAL_FALLBACK" ]; then
    install -m 755 "$PICKER_LOCAL_FALLBACK" "$PICKER_DST"
    ok "Picker installed from local path"
elif curl -fsSL --max-time 10 "$PICKER_URL" -o "$PICKER_DST" 2>/dev/null; then
    chmod +x "$PICKER_DST"
    ok "Picker downloaded to $PICKER_DST"
else
    error_exit "Failed to install sentryusb-pick-binary — daemon won't start without it"
fi

# Run the picker once now so the -current symlink + active-variant file
# exist before systemd tries to start the service.
"$PICKER_DST" || error_exit "sentryusb-pick-binary failed on first run — check journalctl"

# Back-compat symlink at the old path so any third-party tooling or shell
# wrappers referencing /opt/sentryusb/sentryusb keep working.
ln -sfn "$INSTALL_DIR/sentryusb-current" "$INSTALL_DIR/$BINARY_NAME"

# Ensure binary is on PATH
if [ ! -L /usr/local/bin/sentryusb ]; then
    ln -sf "$INSTALL_DIR/sentryusb-current" /usr/local/bin/sentryusb
fi

# ── Step 3: Systemd Service ─────────────────────────────────────────

info "Installing systemd service..."

cat > /etc/systemd/system/sentryusb.service << 'EOF'
[Unit]
Description=SentryUSB Web Server
After=mutable.mount backingfiles.mount
Wants=mutable.mount backingfiles.mount
Conflicts=nginx.service

[Service]
Type=simple
ExecStartPre=-/bin/systemctl stop nginx
ExecStartPre=-/bin/systemctl disable nginx
# Re-pick the best per-CPU binary on every start so a hardware swap
# (re-flashing the SD card into a different Pi) is handled automatically.
ExecStartPre=/usr/local/bin/sentryusb-pick-binary
ExecStart=/opt/sentryusb/sentryusb-current --port 80
Restart=always
RestartSec=5
Environment=RUST_LOG=info
# Cap glibc malloc arenas to 2. Default on multicore ARM is 8× nproc
# arenas, each holding a fragmented heap fork that the kernel never
# reclaims. Steady-state RSS on Pi-class hardware drops ~40-50% with
# this cap, with no measurable throughput impact for our workload.
Environment=MALLOC_ARENA_MAX=2
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable sentryusb
ok "sentryusb.service installed and enabled"

# ── Step 3b: BLE daemon (Python) ───────────────────────────────────

info "Installing SentryUSB BLE daemon..."
BLE_REPO_URL="https://raw.githubusercontent.com/${REPO}/main/server/ble"
# Install at /root/bin/ — this matches both the vendored service unit's
# hardcoded ExecStart path AND what pi-gen 00-run.sh installs, so the same
# binary is reachable whether the user came in via image-flash or
# install-pi.sh. Previously we installed to /opt/sentryusb/ble/ and
# post-patched the service unit with sed — which could silently fail on
# older sed or SELinux-restricted systems, leaving the service pointing at
# a path with no file. The only safe thing is to not transform.
BLE_INSTALL_PATH="/root/bin/sentryusb-ble.py"
mkdir -p /root/bin

if curl -fsSL "$BLE_REPO_URL/sentryusb-ble.py" -o "$BLE_INSTALL_PATH" 2>/dev/null; then
    chmod +x "$BLE_INSTALL_PATH"
    curl -fsSL "$BLE_REPO_URL/sentryusb-ble.service" -o /etc/systemd/system/sentryusb-ble.service 2>/dev/null || true
    curl -fsSL "$BLE_REPO_URL/com.sentryusb.ble.conf" -o /etc/dbus-1/system.d/com.sentryusb.ble.conf 2>/dev/null || true

    apt-get install -y python3-dbus python3-gi bluez >/dev/null 2>&1 || warn "BLE daemon apt deps install failed — the daemon may not start"
    systemctl daemon-reload
    systemctl enable sentryusb-ble 2>/dev/null || true
    # Reload (SIGHUP) — NOT restart. Restarting dbus on Pi OS kills logind,
    # which kills any active SSH session and can wedge the box hard enough
    # to need a power-cycle. Reload picks up the new policy file (which is
    # all we need — dbus rereads /etc/dbus-1/system.d/ on SIGHUP) without
    # dropping any clients.
    systemctl reload dbus 2>/dev/null || true
    ok "BLE daemon installed at $BLE_INSTALL_PATH"
else
    warn "Could not fetch BLE daemon — iOS app pairing will be unavailable"
fi

# ── Step 3c: archiveloop ↔ gadget shim scripts ─────────────────────
#
# archiveloop (shell) calls /root/bin/enable_gadget.sh and disable_gadget.sh
# directly. On a pre-existing Go install those are real configfs scripts; if
# we leave them alone they fight with the Rust handler — two concurrent
# writers to the same /sys/kernel/config/usb_gadget/sentryusb tree produces
# half-configured gadgets that enumerate without exposing LUNs.
#
# Replace them with thin curl shims so archiveloop drives the Rust API
# instead. The shims are idempotent — archiveloop can call enable while we're
# already enabled without side effects.

info "Installing archiveloop gadget shims..."
mkdir -p /root/bin

cat > /root/bin/enable_gadget.sh <<'SHIM'
#!/bin/bash
# Rust SentryUSB shim — archiveloop calls this; we forward to the Rust API.
# Loopback requests bypass the web auth middleware.
exec curl -fsS --max-time 30 -X POST http://127.0.0.1/api/system/gadget-enable
SHIM
chmod +x /root/bin/enable_gadget.sh

cat > /root/bin/disable_gadget.sh <<'SHIM'
#!/bin/bash
exec curl -fsS --max-time 30 -X POST http://127.0.0.1/api/system/gadget-disable
SHIM
chmod +x /root/bin/disable_gadget.sh

ok "Gadget shims installed at /root/bin/{enable,disable}_gadget.sh"

# ── Step 4: Sample Config ───────────────────────────────────────────

if [ ! -f /root/sentryusb.conf ]; then
    info "Creating sample config..."
    # NOTE: this MUST be the Rust port repo (Sentry-USB-Rusty). Earlier
    # versions pointed at the legacy Go repo, so the download silently
    # returned the Go-era sample OR fell back to the tiny offline stub
    # below — both of which left the "raw config editor" in the web UI
    # showing only a handful of keys instead of the full documented set.
    SAMPLE_URL="https://raw.githubusercontent.com/${REPO}/main/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb.conf.sample"
    if curl -fsSL --max-time 15 "$SAMPLE_URL" -o /root/sentryusb.conf; then
        ok "Sample config downloaded to /root/sentryusb.conf"
    else
        # Fallback minimal template if offline/download fails.
        cat > /root/sentryusb.conf << 'CONFEOF'
# SentryUSB Configuration
# Edit these values and run setup from the web UI.
#
# Required:
export CAM_SIZE=30G
#export MUSIC_SIZE=4G
#export LIGHTSHOW_SIZE=1G
#export BOOMBOX_SIZE=100M

# Archive system: none, cifs, nfs, rsync, rclone
#export ARCHIVE_SYSTEM=none

# Optional: WiFi access point (min 8 char password)
#export AP_SSID=SentryUSB
#export AP_PASS=

# Optional: Hostname (default: sentryusb)
#export SENTRYUSB_HOSTNAME=sentryusb

# Optional: External USB drive instead of SD card
#export DATA_DRIVE=

# Optional: Use exFAT instead of FAT32
#export USE_EXFAT=false
CONFEOF
        ok "Sample config created at /root/sentryusb.conf (offline fallback)"
    fi
fi

# ── Step 5: WiFi Marker ────────────────────────────────────────────

if [ ! -f /sentryusb/WIFI_ENABLED ]; then
    touch /sentryusb/WIFI_ENABLED
fi

# ── Step 5b: Hostname + mDNS (sentryusb.local works immediately) ───

TARGET_HOSTNAME="sentryusb"
CURRENT_HOSTNAME=$(hostname -s 2>/dev/null || echo "raspberrypi")

if [ "$CURRENT_HOSTNAME" != "$TARGET_HOSTNAME" ]; then
    info "Setting hostname to ${TARGET_HOSTNAME}..."
    hostnamectl set-hostname "$TARGET_HOSTNAME" 2>/dev/null \
        || echo "$TARGET_HOSTNAME" > /etc/hostname
    # Update /etc/hosts so sudo/local lookups don't warn
    if grep -qE "^127\.0\.1\.1\s" /etc/hosts; then
        sed -i "s/^127\.0\.1\.1\s.*/127.0.1.1\t${TARGET_HOSTNAME}/" /etc/hosts
    else
        echo -e "127.0.1.1\t${TARGET_HOSTNAME}" >> /etc/hosts
    fi
    hostname "$TARGET_HOSTNAME" 2>/dev/null || true
    ok "Hostname set to ${TARGET_HOSTNAME}"
fi

info "Ensuring avahi-daemon is installed for mDNS (${TARGET_HOSTNAME}.local)..."
if ! command -v avahi-daemon >/dev/null 2>&1; then
    apt-get install -y avahi-daemon >/dev/null 2>&1 \
        || warn "avahi-daemon install failed — ${TARGET_HOSTNAME}.local may not resolve"
fi
systemctl enable avahi-daemon >/dev/null 2>&1 || true
systemctl restart avahi-daemon >/dev/null 2>&1 || true
ok "mDNS active: http://${TARGET_HOSTNAME}.local"

# ── Step 6: Start the Service ──────────────────────────────────────

info "Starting SentryUSB..."
systemctl restart sentryusb

# Get IP address for the user — try multiple methods, network may have just bounced
IP=""
for _ in $(seq 1 30); do
    IP=$(hostname -I 2>/dev/null | awk '{print $1}')
    [ -n "$IP" ] && break
    sleep 1
done
HOSTNAME="$TARGET_HOSTNAME"

echo ""
echo -e "${GREEN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║        SentryUSB Installation Complete         ║${NC}"
echo -e "${GREEN}╚════════════════════════════════════════════════╝${NC}"
echo ""
if [ -n "$IP" ]; then
    echo -e "  Web UI:  ${BLUE}http://${IP}${NC}"
else
    echo -e "  Web UI:  ${YELLOW}(no IP detected — check 'ip a' once network is up)${NC}"
fi
echo -e "  mDNS:    ${BLUE}http://${HOSTNAME}.local${NC}"
echo ""
echo -e "  Open the web UI to complete setup via the wizard."
echo -e "  All setup (partitions, drives, etc.) is handled by the binary."
echo ""
echo -e "  Config:  /root/sentryusb.conf"
echo -e "  Binary:  ${INSTALL_DIR}/sentryusb-current → $(readlink "${INSTALL_DIR}/sentryusb-current" 2>/dev/null || echo "<picker has not run yet>")"
echo -e "  Logs:    journalctl -u sentryusb -f"
echo ""
