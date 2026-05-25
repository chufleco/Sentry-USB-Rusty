import { useEffect, useMemo, useState } from "react"
import { useSearchParams } from "react-router-dom"
import type { DriveSummary, RouteOverview } from "@/types/drives"
import { fetchDrives, fetchRouteOverviews } from "@/api/drives"

export const PAGE_SIZE = 5

export type DateRange =
  | { kind: "preset"; preset: DatePreset }
  | { kind: "custom"; start: string; end: string }

export type DatePreset =
  | "today"
  | "yesterday"
  | "last7"
  | "last30"
  | "thisYear"
  | "lastYear"
  | "all"

export interface DrivesFilters {
  origin?: string
  destination?: string
  tag?: string
  minDistanceMi?: number
}

/**
 * Aggregate stats for the currently filtered drive list. Recomputes
 * whenever the date range / filters / underlying drives change so the
 * summary strip always reflects the user's current selection
 * (e.g. "Last 7 days" → last-7-days totals, not lifetime).
 */
export interface DrivesFilteredStats {
  count: number
  totalDistanceMi: number
  totalDistanceKm: number
  totalDurationMs: number
  fsdEngagedMs: number
  fsdDistanceMi: number
  fsdDistanceKm: number
  fsdPercent: number
  fsdDisengagements: number
  autopilotEngagedMs: number
  autopilotPercent: number
  tessieCount: number
}

export interface DrivesListState {
  drives: DriveSummary[]
  visible: DriveSummary[]
  routesByStartTime: Map<string, [number, number][]>
  total: number
  page: number
  pageCount: number
  pageStart: number
  pageEnd: number
  range: DateRange
  filters: DrivesFilters
  sortDir: "asc" | "desc"
  filteredStats: DrivesFilteredStats
  loading: boolean
  error: string | null
  setPage: (n: number) => void
  setRange: (r: DateRange) => void
  setFilters: (f: DrivesFilters) => void
  setSortDir: (d: "asc" | "desc") => void
  refresh: () => Promise<void>
}

function readRange(params: URLSearchParams): DateRange {
  const preset = params.get("range") as DatePreset | null
  const start = params.get("start")
  const end = params.get("end")
  if (start && end) return { kind: "custom", start, end }
  return { kind: "preset", preset: preset ?? "last7" }
}

function readFilters(params: URLSearchParams): DrivesFilters {
  const minStr = params.get("minDist")
  const minDistanceMi = minStr ? Number(minStr) : undefined
  return {
    origin: params.get("origin") || undefined,
    destination: params.get("destination") || undefined,
    tag: params.get("tag") || undefined,
    minDistanceMi: Number.isFinite(minDistanceMi) ? minDistanceMi : undefined,
  }
}

function rangeBounds(range: DateRange, now: Date): { from?: Date; to?: Date } {
  if (range.kind === "custom") {
    return { from: new Date(range.start), to: new Date(range.end) }
  }
  const startOfToday = new Date(now)
  startOfToday.setHours(0, 0, 0, 0)
  switch (range.preset) {
    case "today":
      return { from: startOfToday }
    case "yesterday": {
      const y = new Date(startOfToday)
      y.setDate(y.getDate() - 1)
      return { from: y, to: startOfToday }
    }
    case "last7": {
      const f = new Date(startOfToday)
      f.setDate(f.getDate() - 7)
      return { from: f }
    }
    case "last30": {
      const f = new Date(startOfToday)
      f.setDate(f.getDate() - 30)
      return { from: f }
    }
    case "thisYear": {
      const f = new Date(now.getFullYear(), 0, 1)
      return { from: f }
    }
    case "lastYear": {
      const f = new Date(now.getFullYear() - 1, 0, 1)
      const t = new Date(now.getFullYear(), 0, 1)
      return { from: f, to: t }
    }
    case "all":
    default:
      return {}
  }
}

function filterDrives(
  drives: DriveSummary[],
  range: DateRange,
  filters: DrivesFilters,
  now: Date,
): DriveSummary[] {
  const { from, to } = rangeBounds(range, now)
  return drives.filter((d) => {
    const t = new Date(d.startTime)
    if (from && t < from) return false
    if (to && t >= to) return false
    if (filters.origin && d.startLocation !== filters.origin) return false
    if (filters.destination && d.endLocation !== filters.destination) return false
    if (filters.tag && !(d.tags ?? []).includes(filters.tag)) return false
    if (filters.minDistanceMi !== undefined && d.distanceMi < filters.minDistanceMi) {
      return false
    }
    return true
  })
}

export function useDrivesList(): DrivesListState {
  const [params, setParams] = useSearchParams()
  const [drives, setDrives] = useState<DriveSummary[]>([])
  const [routes, setRoutes] = useState<RouteOverview[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)

  const page = Math.max(1, Number(params.get("page") ?? "1"))
  const sortDir = (params.get("sort") === "asc" ? "asc" : "desc") as "asc" | "desc"
  const range = useMemo(() => readRange(params), [params])
  const filters = useMemo(() => readFilters(params), [params])

  useEffect(() => {
    let cancelled = false
    /* eslint-disable-next-line react-hooks/set-state-in-effect */
    setLoading(true)
    setError(null)
    Promise.all([fetchDrives(), fetchRouteOverviews(20).catch(() => [])])
      .then(([d, r]) => {
        if (cancelled) return
        setDrives(d)
        setRoutes(r)
      })
      .catch((e) => {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [refreshTick])

  const routesByStartTime = useMemo(() => {
    const m = new Map<string, [number, number][]>()
    for (const r of routes) {
      if (r.startTime) m.set(r.startTime, r.points)
    }
    return m
  }, [routes])

  const filtered = useMemo(() => {
    const sorted = [...drives].sort((a, b) => {
      const cmp = new Date(a.startTime).getTime() - new Date(b.startTime).getTime()
      return sortDir === "asc" ? cmp : -cmp
    })
    return filterDrives(sorted, range, filters, new Date())
  }, [drives, range, filters, sortDir])

  const total = filtered.length
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const safePage = Math.min(page, pageCount)
  const pageStart = total === 0 ? 0 : (safePage - 1) * PAGE_SIZE + 1
  const pageEnd = Math.min(total, safePage * PAGE_SIZE)
  const visible = filtered.slice((safePage - 1) * PAGE_SIZE, safePage * PAGE_SIZE)

  // Aggregate stats over the *entire* filtered set (not just the visible
  // page) — this is the "lifetime within current selection" number the
  // header strip displays.
  const filteredStats = useMemo<DrivesFilteredStats>(() => {
    let totalDistanceMi = 0
    let totalDistanceKm = 0
    let totalDurationMs = 0
    let fsdEngagedMs = 0
    let fsdDistanceMi = 0
    let fsdDistanceKm = 0
    let fsdDisengagements = 0
    let autopilotEngagedMs = 0
    let tessieCount = 0

    for (const d of filtered) {
      totalDistanceMi += d.distanceMi
      totalDistanceKm += d.distanceKm
      totalDurationMs += d.durationMs
      fsdEngagedMs += d.fsdEngagedMs
      fsdDistanceMi += d.fsdDistanceMi
      fsdDistanceKm += d.fsdDistanceKm
      fsdDisengagements += d.fsdDisengagements
      autopilotEngagedMs += d.autosteerEngagedMs + d.taccEngagedMs
      if (d.source === "tessie") tessieCount += 1
    }

    const fsdPercent =
      totalDurationMs > 0 ? (fsdEngagedMs / totalDurationMs) * 100 : 0
    const autopilotPercent =
      totalDurationMs > 0 ? (autopilotEngagedMs / totalDurationMs) * 100 : 0

    return {
      count: filtered.length,
      totalDistanceMi,
      totalDistanceKm,
      totalDurationMs,
      fsdEngagedMs,
      fsdDistanceMi,
      fsdDistanceKm,
      fsdPercent,
      fsdDisengagements,
      autopilotEngagedMs,
      autopilotPercent,
      tessieCount,
    }
  }, [filtered])

  const updateParams = (mut: (p: URLSearchParams) => void) => {
    const next = new URLSearchParams(params)
    mut(next)
    setParams(next, { replace: true })
  }

  const setPage = (n: number) => {
    const clamped = Math.max(1, Math.min(pageCount, n))
    updateParams((p) => {
      if (clamped === 1) p.delete("page")
      else p.set("page", String(clamped))
    })
  }

  const setRange = (r: DateRange) => {
    updateParams((p) => {
      p.delete("page")
      p.delete("start")
      p.delete("end")
      p.delete("range")
      if (r.kind === "custom") {
        p.set("start", r.start)
        p.set("end", r.end)
      } else if (r.preset !== "last7") {
        p.set("range", r.preset)
      }
    })
  }

  const setFilters = (f: DrivesFilters) => {
    updateParams((p) => {
      p.delete("page")
      p.delete("origin")
      p.delete("destination")
      p.delete("tag")
      p.delete("minDist")
      if (f.origin) p.set("origin", f.origin)
      if (f.destination) p.set("destination", f.destination)
      if (f.tag) p.set("tag", f.tag)
      if (f.minDistanceMi !== undefined) p.set("minDist", String(f.minDistanceMi))
    })
  }

  const setSortDir = (d: "asc" | "desc") => {
    updateParams((p) => {
      if (d === "desc") p.delete("sort")
      else p.set("sort", "asc")
    })
  }

  const refresh = async () => {
    setRefreshTick((t) => t + 1)
  }

  return {
    drives,
    visible,
    routesByStartTime,
    total,
    page: safePage,
    pageCount,
    pageStart,
    pageEnd,
    range,
    filters,
    sortDir,
    filteredStats,
    loading,
    error,
    setPage,
    setRange,
    setFilters,
    setSortDir,
    refresh,
  }
}
