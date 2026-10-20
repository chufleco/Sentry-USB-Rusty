import { useState, useEffect, useRef, useCallback, type ReactNode } from "react"
import { Toggle } from "@/components/ui/Toggle"
import {
  C6FirmwareModal,
  type C6FirmwareStatus,
} from "@/components/settings/sections/C6FirmwareModal"

interface C6Status {
  present: boolean
  provisioned: boolean
  source_active: boolean
  fw: string | null
  // Live car-link detail so the UI can tell parked vs wedged vs failed-over.
  link: string | null
  owner: string | null
  polling_enabled: boolean | null
  car_ok_age_ms: number | null
}

/**
 * A single row inside the "Tesla BLE" card. When an ESP32-C6 is plugged in, it
 * lets the user pick the ESP32-C6 as the Bluetooth radio for the car link
 * (telemetry + keep-awake ride it), instead of the Pi's own radio. Renders
 * nothing when no ESP32-C6 is detected, so it only appears once one is present.
 *
 * It also surfaces the chip's firmware version and, when a newer build is
 * staged on the Pi, an "update available" affordance that opens the install
 * flow (C6FirmwareModal).
 */
export function C6TelemetryToggle() {
  const [status, setStatus] = useState<C6Status | null>(null)
  const [loaded, setLoaded] = useState(false)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  // Firmware-update status (separate endpoint) + the install modal.
  const [fw, setFw] = useState<C6FirmwareStatus | null>(null)
  // Mirror `fw` in a ref so the status poll can decide whether to keep chasing
  // firmware status after the chip drops off the bus, without re-arming.
  const fwRef = useRef<C6FirmwareStatus | null>(null)
  // User dismissed the "not responding after a flash" recovery banner.
  const [recoveryDismissed, setRecoveryDismissed] = useState(false)
  // When can_recover first went true — the recover banner waits for it to hold a
  // while so it doesn't flash during the brief post-flash heartbeat settle.
  const [canRecoverSince, setCanRecoverSince] = useState<number | null>(null)
  // null = closed; otherwise which flow the modal opens into.
  const [fwModal, setFwModal] = useState<null | "install" | "rollback" | "recover">(null)
  // Mirror `busy` in a ref so the poll interval can read it without re-arming.
  const busyRef = useRef(false)
  useEffect(() => {
    busyRef.current = busy
  }, [busy])
  // Generation token: bumped when the user toggles, so a status poll that was
  // already in flight can't overwrite the result of a newer toggle.
  const genRef = useRef(0)
  const cancelledRef = useRef(false)

  const refreshFw = useCallback(() => {
    fetch("/api/system/c6-firmware")
      .then((r) => (r.ok ? r.json() : null))
      .then((d: C6FirmwareStatus | null) => {
        if (!cancelledRef.current && d) {
          fwRef.current = d
          setFw(d)
        }
      })
      .catch(() => {
        /* transient — keep last-known */
      })
  }, [])

  useEffect(() => {
    cancelledRef.current = false
    const poll = () => {
      const gen = genRef.current
      fetch("/api/system/c6-status")
        // Treat an HTTP error (e.g. 401) as a transient failure, not "no C6".
        .then((r) => {
          if (!r.ok) throw new Error(`c6-status ${r.status}`)
          return r.json()
        })
        .then((d) => {
          // Drop the response if the user toggled while it was in flight, so a
          // stale read can't clobber the fresher toggle result.
          if (cancelledRef.current || gen !== genRef.current) return
          setStatus({
            present: Boolean(d?.present),
            provisioned: Boolean(d?.provisioned),
            source_active: Boolean(d?.source_active),
            fw: typeof d?.fw === "string" && d.fw.trim() ? d.fw.trim() : null,
            link: typeof d?.link === "string" ? d.link : null,
            owner: typeof d?.owner === "string" ? d.owner : null,
            polling_enabled: typeof d?.polling_enabled === "boolean" ? d.polling_enabled : null,
            car_ok_age_ms: typeof d?.car_ok_age_ms === "number" ? d.car_ok_age_ms : null,
          })
          setLoaded(true)
          // Chip healthy again: clear any stale recovery dismissal so a future
          // failed flash still surfaces its banner.
          if (d?.present) setRecoveryDismissed(false)
          // Chase firmware-update status when a C6 is present, OR when the last
          // firmware record is still non-idle — so a flash that drops the chip
          // off the bus keeps reporting (instead of the row silently vanishing).
          if (fwRef.current === null || d?.present || (fwRef.current && fwRef.current.install?.state !== "idle")) {
            refreshFw()
          }
        })
        .catch(() => {
          if (cancelledRef.current || gen !== genRef.current) return
          // Keep the last-known status on a transient fetch error so a present
          // C6's row doesn't vanish mid-session; only a real response changes it.
          setLoaded(true)
        })
    }
    poll()
    // Re-detect while the card is shown so a C6 plugged in (or provisioned)
    // after opening Settings is picked up without a reload. Skip while a toggle
    // request is in flight so a poll can't clobber the optimistic state.
    const id = window.setInterval(() => {
      if (!busyRef.current) poll()
    }, 5000)
    return () => {
      cancelledRef.current = true
      window.clearInterval(id)
    }
  }, [refreshFw])

  useEffect(() => {
    const cr = fw?.can_recover === true
    setCanRecoverSince((prev) => (cr ? prev ?? Date.now() : null))
  }, [fw?.can_recover])

  // Own the modal via fwModal once an op is running, so it persists through the
  // terminal state (success/failed) instead of remounting and resetting to idle.
  useEffect(() => {
    const op = fw?.install?.op
    if (fw?.install?.state === "running" && !fwModal) {
      setFwModal(op === "rollback" ? "rollback" : op === "recover" ? "recover" : "install")
    }
  }, [fw?.install?.state, fw?.install?.op, fwModal])

  async function setSource(useC6: boolean) {
    // Invalidate any in-flight status poll so it can't overwrite this result.
    genRef.current += 1
    setBusy(true)
    setErr(null)
    try {
      const res = await fetch("/api/system/telemetry-source", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ source: useC6 ? "c6_primary" : "sampler" }),
      })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || "Failed to update")
      }
      const data = await res.json().catch(() => ({}))
      // Prefer the server's echoed source; fall back to the requested value.
      const active = data?.source === "c6_primary" || (data?.source == null && useC6)
      setStatus((prev) => (prev ? { ...prev, source_active: active } : prev))
    } catch (e) {
      setErr(e instanceof Error ? e.message : "Failed to update")
    } finally {
      setBusy(false)
    }
  }

  if (!loaded || status === null) {
    return null
  }

  const inst = fw?.install
  const installing = inst?.state === "running"

  // One modal, rendered at a single stable position below — it never remounts
  // across state transitions (which would reset it to idle).
  const modalEl =
    fwModal && fw ? (
      <C6FirmwareModal
        status={fw}
        intent={fwModal}
        onClose={() => {
          setFwModal(null)
          refreshFw()
        }}
        onRefresh={refreshFw}
      />
    ) : null

  let body: ReactNode = null

  if (installing) {
    body = <p className="text-xs text-slate-500">ESP32-C6 firmware: updating…</p>
  } else if (!status.present) {
    const ageSec = inst?.updated_at ? Date.now() / 1000 - inst.updated_at : Infinity
    const wedged =
      !!inst && ageSec < 3600 && (inst.state === "failed" || inst.state === "rolled_back")
    if (wedged && !recoveryDismissed) {
      const wasRecover = inst!.op === "recover"
      body = (
        <div className="flex flex-col gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
          <div className="flex items-center justify-between gap-3">
            <p className="text-xs font-medium text-amber-300">
              {wasRecover
                ? "ESP32-C6 not responding after recovery"
                : "ESP32-C6 not responding after a firmware update"}
            </p>
            <button
              onClick={() => setRecoveryDismissed(true)}
              className="shrink-0 text-[11px] text-amber-400/70 hover:text-amber-300"
            >
              Dismiss
            </button>
          </div>
          <p className="text-[11px] leading-relaxed text-amber-200/80">
            The chip is no longer detected. Unplug the ESP32-C6 and plug it back in,
            or hold its RESET button for ~5 seconds, to recover it.
            {wasRecover
              ? " If recovery had to rewrite the chip, the Tesla key is re-provisioned."
              : " An update or rollback never does a factory write, so the paired Tesla key is intact."}
          </p>
          {inst!.message && <p className="text-[11px] text-amber-200/50">{inst!.message}</p>}
        </div>
      )
    }
  } else {
    const active = status.source_active === true
    const provisioned = status.provisioned === true
    const updateAvailable = fw?.update_available === true
    // Live car-link state so the row shows what the C6 is actually doing.
    // Precedence matters: a stopped poll or a wedged link (up but no signed car
    // response) must show BEFORE owner==="sampler", else a real no-data fault is
    // masked as a benign failover. "session" with a stale car_ok is a wedge.
    const WEDGE_MS = 60000
    const wedge =
      status.link === "session" &&
      (status.car_ok_age_ms === null || status.car_ok_age_ms > WEDGE_MS)
    let linkNote: { text: string; cls: string } | null = null
    if (active) {
      if (status.polling_enabled === false) {
        linkNote = {
          text: "ESP32-C6 is selected but not polling the car (turn Bluetooth on).",
          cls: "text-amber-400/80",
        }
      } else if (wedge) {
        linkNote = {
          text: "ESP32-C6 link is up but the car hasn't responded — check the chip.",
          cls: "text-amber-400/80",
        }
      } else if (status.owner === "sampler") {
        linkNote = {
          text: "Built-in Bluetooth has temporarily taken over (ESP32-C6 couldn't reach the car).",
          cls: "text-amber-400/80",
        }
      } else if (status.link === "session") {
        linkNote = { text: "Connected to the car.", cls: "text-emerald-400/80" }
      } else if (status.link && status.link !== "idle") {
        linkNote = {
          text: `ESP32-C6 link: ${status.link} (not yet connected to the car).`,
          cls: "text-amber-400/80",
        }
      }
    }
    const showRecover =
      fw?.can_recover === true &&
      canRecoverSince !== null &&
      Date.now() - canRecoverSince >= 8000
    body = (
      <>
        {showRecover && (
          <div className="flex flex-col gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
            <p className="text-xs font-medium text-amber-300">ESP32-C6 detected but not responding</p>
            <p className="text-[11px] leading-relaxed text-amber-200/80">
              First try: unplug the ESP32-C6 and plug it back in, or hold its RESET button for ~5
              seconds. If it still does not respond, set it up below — it reinstalls firmware,
              keeping the Tesla key when it can and re-provisioning it only if a full recovery is needed.
            </p>
            <button
              onClick={() => setFwModal("recover")}
              className="self-start rounded-lg bg-amber-500/20 px-2.5 py-1 text-xs font-medium text-amber-300 hover:bg-amber-500/30"
            >
              Set up / recover this ESP32-C6
            </button>
          </div>
        )}
        <Toggle
          checked={active}
          disabled={busy || (!active && !provisioned)}
          onChange={setSource}
          label="Use the ESP32-C6 as the Bluetooth adapter"
          sub={
            !provisioned && !active
              ? "ESP32-C6 detected. Provision it with a Tesla key first, then it can carry the car's Bluetooth."
              : "Use the ESP32-C6 as the car's Bluetooth adapter. If you unplug it, the built-in Bluetooth takes over automatically."
          }
        />
        {linkNote && <p className={`text-[11px] ${linkNote.cls}`}>{linkNote.text}</p>}
        {status.fw && (
          <div className="flex items-center justify-between gap-3">
            <p className="text-xs text-slate-500">ESP32-C6 Firmware: {status.fw}</p>
            {fw &&
              (updateAvailable ? (
                <button
                  onClick={() => setFwModal("install")}
                  className="rounded-lg bg-amber-500/15 px-2.5 py-1 text-xs font-medium text-amber-400 hover:bg-amber-500/25"
                >
                  Update to {fw.candidate_version}
                </button>
              ) : (
                <button
                  onClick={() => setFwModal("rollback")}
                  className="rounded-lg bg-emerald-500/15 px-2.5 py-1 text-xs font-medium text-emerald-400 hover:bg-emerald-500/25"
                >
                  Up to date
                </button>
              ))}
          </div>
        )}
        {updateAvailable && (
          <p className="text-[11px] text-amber-400/80">
            A newer ESP32-C6 firmware ({fw?.candidate_version}) is available.
          </p>
        )}
        {err && <p className="text-xs text-red-400">{err}</p>}
      </>
    )
  }

  return (
    <>
      {body}
      {modalEl}
    </>
  )
}
