import { useEffect, useState } from "react"
import { createPortal } from "react-dom"
import { Plane, Loader2 } from "lucide-react"
import { Modal } from "@/components/ui/Modal"
import { Toggle } from "@/components/ui/Toggle"
import { api } from "@/lib/api"

interface Props {
  onClose: () => void
  /** Reports the latest enabled state so the parent can show its badge. */
  onChange?: (enabled: boolean) => void
}

/**
 * Hidden "secret menu" reached by tapping the Away Mode card icon 5×.
 * Toggles Travel Mode — a single persisted boolean the archive loop reads
 * to keep the USB drive connected to the car while archiving on the road.
 */
export function TravelModeDialog({ onClose, onChange }: Props) {
  const [enabled, setEnabled] = useState(false)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [archiving, setArchiving] = useState(false)

  useEffect(() => {
    let cancelled = false
    api
      .getTravelMode()
      .then((r) => {
        if (!cancelled) setEnabled(r.enabled)
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // Poll archive status while the dialog is open and block toggling during an
  // active archive: flipping Travel Mode mid-cycle would desync the keep-awake
  // start/stop pair for the in-flight cycle. The signal is archiveloop's
  // /tmp/archive_status.json, surfaced as DriveStatus.archiving (is_archiving()).
  useEffect(() => {
    let cancelled = false
    const check = () =>
      api
        .getDriveStatus()
        .then((s) => {
          if (!cancelled) setArchiving(Boolean(s.archiving))
        })
        .catch(() => {})
    check()
    const id = setInterval(check, 4000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  async function toggle(next: boolean) {
    if (archiving) return // toggle is also disabled in the UI; belt-and-suspenders
    setSaving(true)
    setEnabled(next) // optimistic
    try {
      const r = await api.setTravelMode(next)
      setEnabled(r.enabled)
      onChange?.(r.enabled)
    } catch {
      setEnabled(!next) // revert on failure
    } finally {
      setSaving(false)
    }
  }

  // Portal to document.body: the Away Mode card is a `glass-card` with a
  // backdrop-filter, which establishes a containing block for `position:
  // fixed` and would otherwise pin/clip this overlay inside the card.
  return createPortal(
    <Modal
      title={
        <span className="flex items-center gap-2">
          <Plane className="h-4 w-4 text-sky-400" />
          Travel Mode
        </span>
      }
      onClose={onClose}
      size="sm"
    >
      <div className="space-y-4">
        <p className="t-xs">
          For road trips. Keeps archiving footage to your server in the background — over an
          always-on connection like a travel router or VPN — but{" "}
          <span className="font-medium text-slate-200">
            never disconnects the USB drive from the car
          </span>
          , so Sentry &amp; Dashcam keep recording the whole time.
        </p>

        <Toggle
          checked={enabled}
          onChange={toggle}
          disabled={loading || saving || archiving}
          label="Enable Travel Mode"
          sub={
            archiving
              ? "An archive is in progress — wait for it to finish to change Travel Mode."
              : enabled
                ? "On — the drive stays connected to the car while archiving."
                : "Off — normal behavior (the drive briefly disconnects to tidy up after archiving)."
          }
        />

        {archiving && (
          <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
            <Loader2 className="mt-0.5 h-4 w-4 shrink-0 animate-spin text-amber-400" />
            <p className="text-xs text-amber-200/90">
              An archive cycle is running right now. To avoid interrupting it mid-cycle, Travel Mode
              can’t be changed until it finishes — this unlocks automatically in a moment.
            </p>
          </div>
        )}

        <p className="text-[10px] text-slate-500">
          While on, disk cleanup is paused (the car manages its own space) and archiving runs about
          once per snapshot interval. Turn this off when you’re back home to resume normal cleanup.
        </p>
      </div>
    </Modal>,
    document.body,
  )
}
