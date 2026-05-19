#!/bin/bash -eu

LOG_FILE=${LOG_FILE:-/mutable/archiveloop.log}
function log () {
  echo "$( date ):" "$@" >> "$LOG_FILE" 2>/dev/null || true
}

NAME=$(basename "$1")

if [[ "$NAME" != snap-* ]]
then
  log "invalid snapshot name"
  exit
fi

log "releasing snapshot $1"
IMAGE="/backingfiles/snapshots/$NAME/snap.bin"
umount "$IMAGE" || true

# delete the snapshot folders
rm -rf "/backingfiles/snapshots/$NAME"

# delete all obsolete links
find /mutable/TeslaCam/ -lname "*/${NAME}/*" -delete || true

# delete all Sentry, saved and recent folders that are now empty
find /mutable/TeslaCam/ -mindepth 2 -depth -type d -empty -exec rmdir "{}" \; || true
