import { useCallback, useEffect, useState } from "react"

// Electricity rates used to cost charge sessions. Persisted in the
// generic preference store (/api/config/preference) under the keys the
// backend reads in crates/api/src/charging.rs (RateConfig):
//   charging_currency      — symbol string, default "$"
//   charging_default_rate  — flat price per kWh for untagged sessions
//   charging_tag_rates     — { tag: plan } overrides, where each plan is
//                            { flat, schedules } (a flat per-tag rate plus
//                            optional time-of-use schedules). A legacy
//                            value that is a bare number is read as a flat
//                            rate with no schedules, so older configs keep
//                            working.
// Time-of-use is per-tag: a schedule carries a time window, days-of-week,
// and a month range. The session cost itself is computed server-side and
// returned on each session; this hook only reads/writes the inputs.

export interface RateSchedule {
  label: string
  start: string // local "HH:MM"
  end: string // local "HH:MM"; an end before start wraps past midnight
  days: number[] // 0=Sun..6=Sat; empty = every day
  startMonth: number // 1=Jan..12=Dec
  endMonth: number // 1..12; an end month before the start wraps the year
  rate: number
}

// Pricing for one tag: an optional flat fallback rate plus any number of
// time-of-use schedules. An interval is priced at the first schedule that
// covers it, else `flat`, else the global default rate.
export interface TagRate {
  flat: number | null
  schedules: RateSchedule[]
}

export interface ChargingRates {
  currency: string
  defaultRate: number | null
  tags: Record<string, TagRate>
}

const DEFAULT_RATES: ChargingRates = {
  currency: "$",
  defaultRate: null,
  tags: {},
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

const TIME_RE = /^\d{1,2}:\d{2}$/

// Parse a days array (0=Sun..6=Sat) into a sorted, deduped, in-range list.
// Anything else → empty (which the backend treats as "every day").
function parseDays(raw: unknown): number[] {
  if (!Array.isArray(raw)) return []
  const out: number[] = []
  for (const d of raw) {
    const n =
      typeof d === "number" ? d : typeof d === "string" ? parseInt(d, 10) : NaN
    if (Number.isInteger(n) && n >= 0 && n <= 6 && !out.includes(n)) out.push(n)
  }
  return out.sort((a, b) => a - b)
}

// Parse a month (1=Jan..12=Dec) from a number or numeric string, falling
// back when absent or out of range.
function parseMonth(raw: unknown, fallback: number): number {
  const n =
    typeof raw === "number" ? raw : typeof raw === "string" ? parseInt(raw, 10) : NaN
  return Number.isInteger(n) && n >= 1 && n <= 12 ? n : fallback
}

function parseSchedule(raw: unknown): RateSchedule | null {
  if (!raw || typeof raw !== "object") return null
  const o = raw as Record<string, unknown>
  const rate = toRate(o.rate)
  const start = typeof o.start === "string" ? o.start : ""
  const end = typeof o.end === "string" ? o.end : ""
  if (rate == null || !TIME_RE.test(start) || !TIME_RE.test(end)) return null
  return {
    label: typeof o.label === "string" ? o.label : "",
    start,
    end,
    days: parseDays(o.days),
    startMonth: parseMonth(o.startMonth, 1),
    endMonth: parseMonth(o.endMonth, 12),
    rate,
  }
}

// Parse one tag's plan, accepting both the legacy bare-number shape (a
// flat rate, no schedules) and the new { flat, schedules } object.
function parseTagRate(raw: unknown): TagRate {
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    const o = raw as Record<string, unknown>
    const schedules: RateSchedule[] = []
    if (Array.isArray(o.schedules)) {
      for (const s of o.schedules) {
        const parsed = parseSchedule(s)
        if (parsed) schedules.push(parsed)
      }
    }
    return { flat: toRate(o.flat), schedules }
  }
  // Legacy: the value itself is the flat rate.
  return { flat: toRate(raw), schedules: [] }
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
    ]).then(([currency, defaultRate, tagRatesRaw]) => {
      if (cancelled) return
      const tags: Record<string, TagRate> = {}
      if (tagRatesRaw && typeof tagRatesRaw === "object") {
        for (const [k, v] of Object.entries(tagRatesRaw)) {
          const plan = parseTagRate(v)
          // Keep only configured plans (a flat rate or ≥1 schedule).
          if (plan.flat != null || plan.schedules.length > 0) tags[k] = plan
        }
      }
      setRates({
        currency: currency && currency.trim() ? currency.trim() : "$",
        defaultRate: toRate(defaultRate),
        tags,
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
      putPref("charging_tag_rates", next.tags),
    ])
    setRates(next)
  }, [])

  return { rates, loading, save, refresh }
}
