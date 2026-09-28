import { useState, useEffect, useRef } from "react"
import { Toggle } from "@/components/ui/Toggle"

interface C6Status {
  present: boolean
  provisioned: boolean
  source_active: boolean
}

/**
 * A single row inside the "Tesla BLE" card. When an ESP32-C6 is plugged in, it
 * lets the user pick the ESP32-C6 as the Bluetooth radio for the car link
 * (telemetry + keep-awake ride it), instead of the Pi's own radio. Renders
 * nothing when no ESP32-C6 is detected, so it only appears once one is present.
 */
export function C6TelemetryToggle() {
  const [status, setStatus] = useState<C6Status | null>(null)
  const [loaded, setLoaded] = useState(false)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)
  // Mirror `busy` in a ref so the poll interval can read it without re-arming.
  const busyRef = useRef(false)
  useEffect(() => {
    busyRef.current = busy
  }, [busy])
  // Generation token: bumped when the user toggles, so a status poll that was
  // already in flight can't overwrite the result of a newer toggle.
  const genRef = useRef(0)

  useEffect(() => {
    let cancelled = false
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
          if (cancelled || gen !== genRef.current) return
          setStatus({
            present: Boolean(d?.present),
            provisioned: Boolean(d?.provisioned),
            source_active: Boolean(d?.source_active),
          })
          setLoaded(true)
        })
        .catch(() => {
          if (cancelled || gen !== genRef.current) return
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
      cancelled = true
      window.clearInterval(id)
    }
  }, [])

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

  // Only appears once a co-processor is detected, so nothing renders otherwise.
  if (!loaded || status === null || !status.present) {
    return null
  }

  const active = status.source_active === true
  const provisioned = status.provisioned === true

  return (
    <>
      <Toggle
        checked={active}
        // Provisioning gates only ENABLING. An active source must always be
        // switchable off, even if the chip goes unprovisioned or the supervisor
        // is unreachable, so a failing C6 can't strand the car's Bluetooth.
        disabled={busy || (!active && !provisioned)}
        onChange={setSource}
        label="Use the ESP32-C6 Bluetooth radio"
        sub={
          !provisioned && !active
            ? "ESP32-C6 detected. Provision it with a Tesla key first, then it can carry the car's Bluetooth."
            : "Use the ESP32-C6 as the car's Bluetooth adapter. If you unplug it, the built-in Bluetooth takes over automatically."
        }
      />
      {err && <p className="text-xs text-red-400">{err}</p>}
    </>
  )
}
