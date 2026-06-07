import { useEffect, useState } from "react"

// Distance-unit preference (DRIVE_MAP_UNIT in setup config). Returns
// `metric`: true for km, false for mi. Defaults to imperial until the
// config loads so the first paint matches the wizard default.
export function useDistanceUnit(): boolean {
  const [metric, setMetric] = useState(false)
  useEffect(() => {
    let cancelled = false
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        if (cancelled) return
        const entry = cfg?.DRIVE_MAP_UNIT
        if (!entry) return
        const val =
          typeof entry === "object" ? (entry.active ? entry.value : null) : entry
        if (val != null) setMetric(val === "km")
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [])
  return metric
}
