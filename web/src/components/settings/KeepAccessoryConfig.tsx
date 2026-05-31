import { useEffect, useState } from "react"
import { MapPin, Loader2, AlertTriangle } from "lucide-react"
import { cn } from "@/lib/utils"
import { KeepAccessoryMap } from "@/components/settings/KeepAccessoryMap"

export interface KeepAccessoryValues {
  enabled: boolean
  homeLat: number | null
  homeLon: number | null
  radiusM: number
}

const RADIUS_PRESETS = [50, 100, 200, 500]
const RADIUS_MIN = 20
const RADIUS_MAX = 2000

/**
 * Shared, controlled keep-accessory config form — used by both the setup
 * wizard and the Settings card. Pure presentation: the parent owns the
 * values and the persistence. `onUseCurrentLocation` is optional (the
 * setup wizard may run before BLE is paired); when provided it fetches the
 * car's last GPS fix to set the home geofence center.
 */
export function KeepAccessoryConfig({
  values,
  onChange,
  onUseCurrentLocation,
  checkKeepAwake = false,
}: {
  values: KeepAccessoryValues
  onChange: (patch: Partial<KeepAccessoryValues>) => void
  onUseCurrentLocation?: () => Promise<{ lat: number; lon: number } | null>
  /**
   * When true (Settings context), live-check whether "Use BLE for
   * keep-awake" is on and warn if it isn't — the home→OFF release needs
   * the car reachable over BLE through the archive, or accessory power
   * can stay stuck ON at home. Off in the setup wizard (the BLE keep-awake
   * toggle lives right there in the same step).
   */
  checkKeepAwake?: boolean
}) {
  const [locating, setLocating] = useState(false)
  const [locError, setLocError] = useState<string | null>(null)
  // Keep-awake dependency: null = unknown/loading, true/false = saved state.
  const [keepAwakeOn, setKeepAwakeOn] = useState<boolean | null>(null)
  const [enablingKa, setEnablingKa] = useState(false)

  useEffect(() => {
    if (!checkKeepAwake || !values.enabled) return
    let alive = true
    fetch("/api/system/ble-keep-awake-enabled")
      .then((r) => r.json())
      .then((d) => {
        if (alive) setKeepAwakeOn(Boolean(d?.enabled))
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [checkKeepAwake, values.enabled])

  async function enableKeepAwake() {
    setEnablingKa(true)
    try {
      const r = await fetch("/api/system/ble-keep-awake-enabled", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: true }),
      })
      if (r.ok) setKeepAwakeOn(true)
    } catch {
      /* leave the warning up; user can retry */
    } finally {
      setEnablingKa(false)
    }
  }
  // Local text state for the radius so you can clear the field and type any
  // number freely — we only clamp to [20, 2000] when you finish (blur/Enter),
  // instead of fighting every keystroke.
  const [radiusText, setRadiusText] = useState(String(values.radiusM))
  useEffect(() => {
    setRadiusText(String(values.radiusM))
  }, [values.radiusM])

  function commitRadius() {
    const n = Math.round(Number(radiusText))
    if (!Number.isFinite(n) || radiusText.trim() === "") {
      setRadiusText(String(values.radiusM)) // revert junk/empty to last good
      return
    }
    const clamped = Math.min(RADIUS_MAX, Math.max(RADIUS_MIN, n))
    setRadiusText(String(clamped))
    if (clamped !== values.radiusM) onChange({ radiusM: clamped })
  }

  async function useCurrent() {
    if (!onUseCurrentLocation) return
    setLocating(true)
    setLocError(null)
    try {
      const fix = await onUseCurrentLocation()
      if (fix) onChange({ homeLat: fix.lat, homeLon: fix.lon })
      else
        setLocError(
          "No GPS fix yet — enable this, make sure BLE is paired, then park at home and wait ~30s.",
        )
    } catch {
      setLocError("Couldn't read the car's location.")
    } finally {
      setLocating(false)
    }
  }

  const haveHome = values.homeLat != null && values.homeLon != null

  return (
    <div className="space-y-3">
      {/* 12V power gate — the whole feature is off unless this is on */}
      <label
        className={cn(
          "flex cursor-pointer items-start gap-3 rounded-lg border p-3 transition-colors",
          values.enabled
            ? "border-blue-500/40 bg-blue-500/10"
            : "border-white/5 bg-white/[0.02] hover:border-white/10",
        )}
      >
        <input
          type="checkbox"
          checked={values.enabled}
          onChange={(e) => onChange({ enabled: e.target.checked })}
          className="mt-0.5 accent-blue-500"
        />
        <div>
          <p className="text-sm font-medium text-slate-300">
            My Pi is powered from the 12V / cigarette-lighter outlet
          </p>
          <p className="mt-0.5 text-xs text-slate-600">
            Enable only if your Pi runs off the 12V accessory outlet — NOT the glovebox USB
            (the car powers that itself during Sentry). When on, the Pi automatically keeps
            "Keep Accessory Power" enabled while you're parked away from home, so Sentry keeps
            recording with the Pi alive.
          </p>
        </div>
      </label>

      {/* Keep-awake dependency warning — the home→OFF release needs the car
          reachable over BLE through the archive. */}
      {checkKeepAwake && values.enabled && keepAwakeOn === false && (
        <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
          <div className="space-y-2">
            <p className="text-xs text-amber-200/90">
              <span className="font-medium">Turn on “Use BLE for keep-awake” too.</span> It keeps
              the car reachable over BLE through the archive so the Pi can power down cleanly at
              home. Without it, accessory power can stay stuck on at home (battery drain).
            </p>
            <button
              type="button"
              onClick={enableKeepAwake}
              disabled={enablingKa}
              className="inline-flex items-center gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/15 px-2.5 py-1 text-xs font-medium text-amber-100 transition-colors hover:bg-amber-500/25 disabled:opacity-50"
            >
              {enablingKa && <Loader2 className="h-3 w-3 animate-spin" />}
              Turn on keep-awake
            </button>
          </div>
        </div>
      )}

      {/* Home geofence — only relevant once the feature is enabled */}
      {values.enabled && (
        <div className="space-y-3 rounded-lg border border-white/5 bg-white/[0.02] p-3">
          <div className="flex items-center justify-between gap-2">
            <p className="text-sm font-medium text-slate-300">Home location</p>
            {onUseCurrentLocation && (
              <button
                type="button"
                onClick={useCurrent}
                disabled={locating}
                className="inline-flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1 text-xs text-slate-200 transition-colors hover:border-blue-500/40 disabled:opacity-50"
              >
                {locating ? (
                  <Loader2 className="h-3 w-3 animate-spin" />
                ) : (
                  <MapPin className="h-3 w-3" />
                )}
                Use current location
              </button>
            )}
          </div>
          <KeepAccessoryMap
            lat={values.homeLat}
            lon={values.homeLon}
            radiusM={values.radiusM}
            onPlace={(la, lo) => onChange({ homeLat: la, homeLon: lo })}
          />
          <p className="text-xs text-slate-600">
            Tap the map (or drag the pin) to set your home — the blue circle is your radius.
            Anywhere outside it counts as away → Keep Accessory Power turns on automatically.
            {onUseCurrentLocation
              ? " Or tap “Use current location” to use the car’s GPS."
              : ""}
          </p>
          {haveHome ? (
            <p className="text-xs text-slate-400">
              📍 {values.homeLat!.toFixed(5)}, {values.homeLon!.toFixed(5)}
            </p>
          ) : (
            <p className="text-xs text-amber-400/80">
              No home set — tap the map to drop your home pin.
            </p>
          )}
          {locError && <p className="text-xs text-red-400">{locError}</p>}

          {/* Adjustable radius — number input + quick presets */}
          <div>
            <label className="mb-1 block text-xs font-medium text-slate-400">
              Radius (meters)
            </label>
            <div className="flex flex-wrap items-center gap-2">
              <input
                type="number"
                inputMode="numeric"
                min={RADIUS_MIN}
                max={RADIUS_MAX}
                step={10}
                value={radiusText}
                onChange={(e) => setRadiusText(e.target.value)}
                onBlur={commitRadius}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur()
                }}
                className="w-24 rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
              />
              <div className="flex gap-1">
                {RADIUS_PRESETS.map((r) => (
                  <button
                    key={r}
                    type="button"
                    onClick={() => onChange({ radiusM: r })}
                    className={cn(
                      "rounded-md border px-2 py-1 text-xs transition-colors",
                      values.radiusM === r
                        ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                        : "border-white/10 bg-white/5 text-slate-400 hover:border-white/20",
                    )}
                  >
                    {r}m
                  </button>
                ))}
              </div>
            </div>
            <p className="mt-1 text-xs text-slate-600">
              The circle on the map resizes as you change this. Increase if your home sometimes
              reads as a neighbor's address.
            </p>
          </div>
        </div>
      )}
    </div>
  )
}
