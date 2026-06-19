import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react"

export type AwayModeKind = "manual" | "auto"

interface AwayModeStatus {
    /** Automation mode (backend source of truth). Undefined until the
     *  first status poll resolves (or an older backend). */
    mode?: AwayModeKind
    state: "idle" | "active"
    has_rtc?: boolean
    /** False when no AP profile exists (AP unchecked/removed in setup).
     *  Undefined until the first status poll resolves. */
    ap_configured?: boolean
    ap_ssid?: string
    ap_ip?: string
    // Manual timer
    expires_at?: string
    remaining_sec?: number
    enabled_at?: string
    // Automatic geofence
    /** Committed home/away decision: true=home, false=away, null=undecided. */
    is_home?: boolean | null
    /** Whether the AP is currently commanded on (auto mode). */
    ap_on?: boolean
    /** Whether a home geofence center is set. */
    geofence_configured?: boolean
    /** Whether BLE telemetry (the GPS source) is enabled. */
    ble_ready?: boolean
    /** Last GPS fix is missing or too old to act on. */
    gps_stale?: boolean
    last_fix_age_sec?: number | null
    home_lat?: number | null
    home_lon?: number | null
    home_radius_m?: number
}

/** Home geofence values for the Automatic-mode editor. */
export interface AwayGeofenceValues {
    homeLat: number | null
    homeLon: number | null
    radiusM: number
}

interface AwayModeContextValue {
    status: AwayModeStatus
    enable: (durationMin: number) => Promise<void>
    disable: () => Promise<void>
    setMode: (mode: AwayModeKind) => Promise<void>
    config: AwayGeofenceValues
    updateConfig: (patch: Partial<AwayGeofenceValues>) => void
    useCurrentLocation: () => Promise<{ lat: number; lon: number } | null>
}

const DEFAULT_CONFIG: AwayGeofenceValues = { homeLat: null, homeLon: null, radiusM: 120 }

const AwayModeContext = createContext<AwayModeContextValue>({
    status: { state: "idle" },
    enable: async () => { },
    disable: async () => { },
    setMode: async () => { },
    config: DEFAULT_CONFIG,
    updateConfig: () => { },
    useCurrentLocation: async () => null,
})

export function useAwayMode() {
    return useContext(AwayModeContext)
}

const AWAY_MODE_LS_KEY = "sentryusb_away_mode"

/** Returns locally stored Away Mode info (survives connection loss). */
export function getStoredAwayMode(): { enabled_at: string; ap_ssid: string; ap_ip: string } | null {
    try {
        const raw = localStorage.getItem(AWAY_MODE_LS_KEY)
        if (!raw) return null
        return JSON.parse(raw)
    } catch {
        return null
    }
}

export function AwayModeProvider({ children }: { children: React.ReactNode }) {
    const [status, setStatus] = useState<AwayModeStatus>({ state: "idle" })
    const [config, setConfig] = useState<AwayGeofenceValues>(DEFAULT_CONFIG)
    const lastMutation = useRef(0)
    const saveTimer = useRef<number | null>(null)

    const refresh = useCallback(async () => {
        const startedAt = Date.now()
        try {
            const res = await fetch("/api/away-mode/status")
            const data: AwayModeStatus = await res.json()
            if (startedAt >= lastMutation.current) {
                setStatus(data)
                if (data.state === "idle" && data.mode !== "auto") {
                    localStorage.removeItem(AWAY_MODE_LS_KEY)
                }
            }
        } catch { /* ignore — connection may be lost due to Away Mode */ }
    }, [])

    // Poll status.
    useEffect(() => {
        let mounted = true
        const tick = () => { if (mounted) refresh() }
        tick()
        const iv = setInterval(tick, 5000)
        return () => { mounted = false; clearInterval(iv) }
    }, [refresh])

    // Load the geofence config once on mount.
    useEffect(() => {
        let alive = true
        fetch("/api/away-mode/config")
            .then((r) => r.json())
            .then((d) => {
                if (!alive) return
                setConfig({
                    homeLat: typeof d.home_lat === "number" ? d.home_lat : null,
                    homeLon: typeof d.home_lon === "number" ? d.home_lon : null,
                    radiusM: typeof d.home_radius_m === "number" ? d.home_radius_m : 120,
                })
            })
            .catch(() => { })
        return () => { alive = false }
    }, [])

    // Clear the pending debounced config PUT on unmount.
    useEffect(() => () => {
        if (saveTimer.current) clearTimeout(saveTimer.current)
    }, [])

    const enable = useCallback(async (durationMin: number) => {
        lastMutation.current = Date.now()
        try {
            const res = await fetch("/api/away-mode/enable", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ duration_min: durationMin }),
            })
            const data: AwayModeStatus = await res.json()
            setStatus(data)
            // Store in localStorage so the connection banner can reference it
            // even after the API becomes unreachable.
            localStorage.setItem(AWAY_MODE_LS_KEY, JSON.stringify({
                enabled_at: data.enabled_at ?? new Date().toISOString(),
                ap_ssid: data.ap_ssid ?? "",
                ap_ip: data.ap_ip ?? "",
            }))
        } catch { /* ignore */ }
    }, [])

    const disable = useCallback(async () => {
        lastMutation.current = Date.now()
        setStatus((prev) => ({ ...prev, state: "idle" }))
        localStorage.removeItem(AWAY_MODE_LS_KEY)
        try {
            await fetch("/api/away-mode", { method: "DELETE" })
        } catch { /* ignore */ }
    }, [])

    const setMode = useCallback(async (mode: AwayModeKind) => {
        lastMutation.current = Date.now()
        setStatus((prev) => ({ ...prev, mode }))
        try {
            await fetch("/api/away-mode/mode", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ mode }),
            })
        } catch { /* ignore */ }
        // Pull fresh full status (the /mode response omits the ap_ssid/ap_ip
        // the status endpoint adds). refresh() starts after this mutation's
        // timestamp, so its own response still applies — but we must NOT
        // reset the guard to 0: doing so let a status poll that was already
        // in flight before the switch land afterward and revert the
        // optimistic mode, flickering the picker back for one cycle.
        refresh()
    }, [refresh])

    const updateConfig = useCallback((patch: Partial<AwayGeofenceValues>) => {
        setConfig((prev) => {
            const next = { ...prev, ...patch }
            if (saveTimer.current) clearTimeout(saveTimer.current)
            // Debounce — a PUT triggers a RO-root remount on the Pi, so we
            // don't want one per keystroke (mirrors useKeepAccessory).
            saveTimer.current = window.setTimeout(() => {
                fetch("/api/away-mode/config", {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({
                        home_lat: next.homeLat,
                        home_lon: next.homeLon,
                        home_radius_m: next.radiusM,
                    }),
                }).catch(() => { })
            }, 600)
            return next
        })
    }, [])

    /** Fetch the car's last GPS fix (for the "Use current location" button). */
    const useCurrentLocation = useCallback(async (): Promise<{ lat: number; lon: number } | null> => {
        try {
            const d = await fetch("/api/system/keep-accessory-gps").then((r) => r.json())
            if (typeof d.lat === "number" && typeof d.lon === "number") {
                return { lat: d.lat, lon: d.lon }
            }
            return null
        } catch {
            return null
        }
    }, [])

    return (
        <AwayModeContext.Provider
            value={{ status, enable, disable, setMode, config, updateConfig, useCurrentLocation }}
        >
            {children}
        </AwayModeContext.Provider>
    )
}
