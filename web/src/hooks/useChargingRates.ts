import { useCallback, useEffect, useState } from "react"

// Electricity rates used to cost charge sessions. Persisted in the
// generic preference store (/api/config/preference) under three keys the
// backend reads in crates/api/src/charging.rs (RateConfig):
//   charging_currency      — symbol string, default "$"
//   charging_default_rate  — price per kWh for untagged / fallback
//   charging_tag_rates     — { tag: price-per-kWh } overrides
// The session cost itself is computed server-side and returned on each
// session; this hook only reads/writes the inputs.

export interface ChargingRates {
  currency: string
  defaultRate: number | null
  tagRates: Record<string, number>
}

const DEFAULT_RATES: ChargingRates = {
  currency: "$",
  defaultRate: null,
  tagRates: {},
}

async function getPref<T>(key: string): Promise<T | null> {
  try {
    const res = await fetch(
      `/api/config/preference?key=${encodeURIComponent(key)}`,
    )
    if (!res.ok) return null
    const data = await res.json()
    const v = data?.value
    return v === null || v === undefined ? null : (v as T)
  } catch {
    return null
  }
}

async function putPref(key: string, value: unknown): Promise<void> {
  await fetch("/api/config/preference", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key, value }),
  })
}

// Prefs can come back as a number or a numeric string; normalise to a
// finite, non-negative number or null.
function toRate(v: unknown): number | null {
  const n =
    typeof v === "number"
      ? v
      : typeof v === "string"
        ? parseFloat(v.trim())
        : NaN
  return Number.isFinite(n) && n >= 0 ? n : null
}

export function useChargingRates() {
  const [rates, setRates] = useState<ChargingRates>(DEFAULT_RATES)
  const [loading, setLoading] = useState(true)
  const [reloadKey, setReloadKey] = useState(0)

  const refresh = useCallback(() => setReloadKey((k) => k + 1), [])

  useEffect(() => {
    let cancelled = false
    Promise.all([
      getPref<string>("charging_currency"),
      getPref<number | string>("charging_default_rate"),
      getPref<Record<string, unknown>>("charging_tag_rates"),
    ]).then(([currency, defaultRate, tagRates]) => {
      if (cancelled) return
      const cleanTagRates: Record<string, number> = {}
      if (tagRates && typeof tagRates === "object") {
        for (const [k, v] of Object.entries(tagRates)) {
          const n = toRate(v)
          if (n != null) cleanTagRates[k] = n
        }
      }
      setRates({
        currency: currency && currency.trim() ? currency.trim() : "$",
        defaultRate: toRate(defaultRate),
        tagRates: cleanTagRates,
      })
      setLoading(false)
    })
    return () => {
      cancelled = true
    }
  }, [reloadKey])

  const save = useCallback(async (next: ChargingRates) => {
    await Promise.all([
      putPref("charging_currency", next.currency || "$"),
      putPref("charging_default_rate", next.defaultRate),
      putPref("charging_tag_rates", next.tagRates),
    ])
    setRates(next)
  }, [])

  return { rates, loading, save, refresh }
}
