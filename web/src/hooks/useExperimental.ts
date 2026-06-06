import { useEffect, useState } from "react"

// Reads the master experimental opt-in (SENTRYUSB_EXPERIMENTAL) from
// the same /api/setup/config the rest of the app uses. Returns null
// while the fetch is in flight so callers can distinguish "not yet
// known" from a definite false and avoid flashing experimental UI on
// during the initial load. Mirrors the config-entry shape Drives.tsx
// already consumes ({ value, active } | string).
//
// Module-scope cache so every consumer (sidebar, mobile nav, pages)
// shares one fetch instead of each issuing its own round trip on mount.
let cached: boolean | null = null
let inflight: Promise<boolean> | null = null

function readExperimental(): Promise<boolean> {
  if (cached !== null) return Promise.resolve(cached)
  if (inflight) return inflight
  inflight = fetch("/api/setup/config")
    .then((r) => r.json())
    .then((cfg) => {
      const entry = cfg?.SENTRYUSB_EXPERIMENTAL
      const raw =
        typeof entry === "object"
          ? entry?.active
            ? entry.value
            : null
          : entry
      const on =
        typeof raw === "string" &&
        ["yes", "true", "1"].includes(raw.trim().toLowerCase())
      cached = on
      return on
    })
    .catch(() => {
      cached = false
      return false
    })
    .finally(() => {
      inflight = null
    })
  return inflight
}

export function useExperimental(): boolean | null {
  const [enabled, setEnabled] = useState<boolean | null>(cached)
  useEffect(() => {
    let cancelled = false
    readExperimental().then((on) => {
      if (!cancelled) setEnabled(on)
    })
    return () => {
      cancelled = true
    }
  }, [])
  return enabled
}
