#!/bin/bash -eu

ARCHIVE_HOST_NAME="$1"

# Probe timeouts default to 1s (snappy, normal mode). Travel Mode raises them
# via env (exported by the watchdog in archive-clips.sh) so a slow/relayed VPN
# link isn't misread as "unreachable".
ping -q -w "${ARCHIVE_PING_TIMEOUT:-1}" -c 1 "$ARCHIVE_HOST_NAME" &> /dev/null \
  || ssh -q -o ConnectTimeout="${ARCHIVE_SSH_TIMEOUT:-1}" "$RSYNC_USER"@"$ARCHIVE_HOST_NAME" exit
