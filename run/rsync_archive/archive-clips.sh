#!/bin/bash -eu

# Connection monitor: poll the archive server every ~10s. Five
# consecutive misses kill rsync (and this script) so archiveloop can
# reach `connect_usb_drives_to_host` and put the gadget back online
# instead of hanging on a dropped SSH socket while the user drives away.
# rsync's `--timeout=600` only fires on socket-idle, not on a quietly-
# dropping link, so a bash-level monitor is the only way to bound the
# hang from outside the rsync process.
#
# Travel Mode (passed fresh by archiveloop as TRAVEL_MODE_ACTIVE) relaxes the
# thresholds below for slow, high-latency VPN links so a still-progressing
# transfer isn't killed by a brief mobile-link hiccup. Normal mode keeps the
# original snappy values byte-for-byte so "drive away from home" recovery is
# unchanged.
if [ "${TRAVEL_MODE_ACTIVE:-0}" = "1" ]; then
  MONITOR_MISSES=20            # ~minutes of sustained loss before giving up
  MONITOR_TIMEOUT=20           # must exceed the patient probe (ping 4 + ssh 8 ~= 12s)
  export ARCHIVE_PING_TIMEOUT=4 ARCHIVE_SSH_TIMEOUT=8
  RSYNC_EXTRA=(--partial)      # resume an interrupted large clip next cycle
else
  MONITOR_MISSES=5             # unchanged
  MONITOR_TIMEOUT=6            # unchanged
  RSYNC_EXTRA=()               # unchanged (expands to nothing)
fi

function connectionmonitor {
  while true
  do
    for (( i = 1; i <= MONITOR_MISSES; i++ ))
    do
      if timeout "$MONITOR_TIMEOUT" /root/bin/archive-is-reachable.sh "$ARCHIVE_SERVER"
      then
        sleep 5
        continue 2
      fi
      sleep 1
    done
    log "connection dead, killing archive-clips"
    # Give rsync a chance to delete the source files it already copied
    # before we kill it hard.
    killall rsync || true
    sleep 2
    killall -9 rsync || true
    kill -9 "$1" || true
    return
  done
}

connectionmonitor $$ &

while [ -n "${1+x}" ]
do
  if ! (rsync -avhRL --timeout=600 --remove-source-files --no-perms --omit-dir-times \
        ${RSYNC_EXTRA[@]+"${RSYNC_EXTRA[@]}"} \
        --stats --log-file=/tmp/archive-rsync-cmd.log --ignore-missing-args \
        --files-from="$2" "$1" "$RSYNC_USER@$RSYNC_SERVER:$RSYNC_PATH" &> /tmp/rsynclog || [[ "$?" = "24" ]] )
  then
    cat /tmp/archive-rsync-cmd.log /tmp/rsynclog > /tmp/archive-error.log
    kill %1 || true
    exit 1
  fi
  shift 2
done

# Stop the monitor.
kill %1 || true
