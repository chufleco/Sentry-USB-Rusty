import { useCallback, useEffect, useMemo, useState } from "react"
import { Link } from "react-router-dom"
import { BatteryCharging, ChevronRight, Loader2, MapPin, Zap } from "lucide-react"
import {
  fetchChargeSessions,
  fetchChargeTags,
  fetchCurrentCharge,
  setChargeTags,
} from "@/api/charging"
import type { ChargeSessionSummary, CurrentCharge } from "@/types/charging"
import { DatePopover } from "@/components/drives/DatePopover"
import { TagPopover } from "@/components/drives/TagPopover"
import {
  ChargingSummaryStrip,
  type ChargingStats,
} from "@/components/charging/ChargingSummaryStrip"
import { ChargingTagFilter } from "@/components/charging/ChargingTagFilter"
import { ChargingRatesButton } from "@/components/charging/ChargingRatesButton"
import { MiniPinMap } from "@/components/charging/MiniPinMap"
import { rangeBounds, type DateRange } from "@/hooks/useDrivesList"
import { useDistanceUnit } from "@/hooks/useDistanceUnit"
import { fmtDuration, fmtEnergy, fmtMoney, fmtSoc } from "@/lib/charge-format"

// While a charge is in progress, refresh on this cadence so the active
// session grows in place instead of only settling once the charge ends.
const POLL_MS = 30_000

export default function Charging() {
  const [sessions, setSessions] = useState<ChargeSessionSummary[]>([])
  const [tags, setTags] = useState<string[]>([])
  const [selectedTags, setSelectedTags] = useState<string[]>([])
  const [current, setCurrent] = useState<CurrentCharge | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const metric = useDistanceUnit()
  // Charge sessions are infrequent, so default to All time rather than
  // the Drives default of Last 7 days (which would usually be empty).
  const [range, setRange] = useState<DateRange>({ kind: "preset", preset: "all" })

  // Refresh sessions + tags + live status. Used after a tag edit or a
  // rate change so server-computed cost and the tag vocabulary stay in
  // sync.
  const reload = useCallback(async () => {
    const [s, t, c] = await Promise.all([
      fetchChargeSessions(),
      fetchChargeTags(),
      fetchCurrentCharge().catch(() => null),
    ])
    setSessions(s)
    setTags(t)
    if (c) setCurrent(c)
  }, [])

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    Promise.all([
      fetchChargeSessions(),
      fetchChargeTags(),
      fetchCurrentCharge().catch(() => null),
    ])
      .then(([s, t, c]) => {
        if (cancelled) return
        setSessions(s)
        setTags(t)
        if (c) setCurrent(c)
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // Poll live status; while actively charging, also refresh the session
  // list so the in-progress charge updates without a manual reload.
  useEffect(() => {
    const id = setInterval(async () => {
      if (document.hidden) return
      const c = await fetchCurrentCharge().catch(() => null)
      if (!c) return
      setCurrent(c)
      if (c.charging) {
        const [s, t] = await Promise.all([
          fetchChargeSessions(),
          fetchChargeTags(),
        ])
        setSessions(s)
        setTags(t)
      }
    }, POLL_MS)
    return () => clearInterval(id)
  }, [])

  const onTagsChange = useCallback(
    async (id: number, next: string[]) => {
      // Optimistic: show the new tags immediately, then resync (cost is
      // recomputed server-side from the tags).
      setSessions((prev) =>
        prev.map((s) => (s.id === id ? { ...s, tags: next } : s)),
      )
      try {
        await setChargeTags(id, next)
      } finally {
        await reload()
      }
    },
    [reload],
  )

  // The newest session is the in-progress one while the car reports
  // charging (sessions come back newest-first).
  const activeId = current?.charging ? (sessions[0]?.id ?? null) : null

  const visible = useMemo(() => {
    const { from, to } = rangeBounds(range, new Date())
    return sessions.filter((s) => {
      const t = new Date(s.startMs)
      if (from && t < from) return false
      if (to && t >= to) return false
      if (
        selectedTags.length > 0 &&
        !s.tags.some((tag) => selectedTags.includes(tag))
      )
        return false
      return true
    })
  }, [sessions, range, selectedTags])

  const stats: ChargingStats = useMemo(() => {
    const costs = visible
      .map((s) => s.cost)
      .filter((c): c is number => c != null)
    const effs = visible
      .map((s) => s.efficiencyPct)
      .filter((e): e is number => e != null)
    return {
      count: visible.length,
      totalEnergyKwh: visible.reduce((sum, s) => sum + (s.energyAddedKwh ?? 0), 0),
      totalDurationSecs: visible.reduce((sum, s) => sum + s.durationSecs, 0),
      totalCost: costs.length ? costs.reduce((a, b) => a + b, 0) : null,
      currency: visible.find((s) => s.currency)?.currency ?? "$",
      avgEfficiency: effs.length
        ? effs.reduce((a, b) => a + b, 0) / effs.length
        : null,
    }
  }, [visible])

  return (
    <div className="mx-auto w-full max-w-5xl px-4 py-6 sm:px-6 sm:py-8">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3 sm:mb-6">
        <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">
          Charging
        </h1>
      </div>

      <div className="mb-4 flex flex-wrap items-center gap-x-3 gap-y-3">
        <DatePopover range={range} onChange={setRange} />
        <ChargingTagFilter
          tags={tags}
          selected={selectedTags}
          onChange={setSelectedTags}
        />
        <ChargingRatesButton tags={tags} onSaved={reload} />
        <ChargingSummaryStrip stats={stats} loading={loading} />
      </div>

      <div className="flex flex-col gap-3">
        {loading && (
          <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading charging history…
          </div>
        )}
        {error && !loading && (
          <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
            Failed to load charging history: {error}
          </div>
        )}
        {!loading && !error && visible.length === 0 && (
          <div className="rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-center text-sm text-slate-400">
            <BatteryCharging className="mx-auto mb-3 h-8 w-8 text-slate-600" />
            {sessions.length === 0
              ? "No charging sessions recorded yet. Sessions appear here once the car charges while the Pi is sampling."
              : "No charging sessions match these filters."}
          </div>
        )}
        {!loading &&
          visible.map((s) => (
            <ChargeRow
              key={s.id}
              session={s}
              metric={metric}
              active={s.id === activeId}
              livePowerKw={s.id === activeId ? (current?.powerKw ?? null) : null}
              onTagsChange={onTagsChange}
            />
          ))}
      </div>
    </div>
  )
}

function ChargeRow({
  session,
  metric,
  active,
  livePowerKw,
  onTagsChange,
}: {
  session: ChargeSessionSummary
  metric: boolean
  active: boolean
  livePowerKw: number | null
  onTagsChange: (id: number, tags: string[]) => Promise<void> | void
}) {
  const start = new Date(session.startMs)
  // Two forms so the SoC range degrades instead of vanishing when the
  // row is tight: `socShort` is always shown ("62% → 79%"); the range
  // ("62% (132 mi) → …") only appears when there's room (sm+).
  const socShort =
    session.startSoc != null && session.endSoc != null
      ? `${fmtSoc(session.startSoc)} → ${fmtSoc(session.endSoc)}`
      : session.endSoc != null
        ? fmtSoc(session.endSoc)
        : "—"
  const socPart = (pct: number | null, mi: number | null): string => {
    if (pct == null) return "—"
    if (mi == null) return fmtSoc(pct)
    const dist = metric ? `${Math.round(mi * 1.609344)} km` : `${Math.round(mi)} mi`
    return `${fmtSoc(pct)} (${dist})`
  }
  const socFull =
    session.startSoc != null && session.endSoc != null
      ? `${socPart(session.startSoc, session.startRangeMi)} → ${socPart(session.endSoc, session.endRangeMi)}`
      : session.endSoc != null
        ? socPart(session.endSoc, session.endRangeMi)
        : "—"

  return (
    <Link
      to={`/charging/${session.id}`}
      className={
        "group flex items-center gap-3 rounded-2xl border p-3 transition-colors sm:gap-4 sm:p-4 " +
        (active
          ? "border-emerald-400/30 bg-emerald-500/10 hover:bg-emerald-500/15"
          : "border-white/[0.06] bg-white/[0.025] hover:border-white/10 hover:bg-white/[0.04]")
      }
    >
      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-300 ring-1 ring-inset ring-emerald-500/20">
        <BatteryCharging className={"h-5 w-5" + (active ? " animate-pulse" : "")} />
      </span>

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5 text-sm font-medium text-slate-100">
          {active && (
            <span className="inline-flex shrink-0 items-center gap-1 rounded-full bg-emerald-500/15 px-1.5 py-0.5 text-[10px] font-medium text-emerald-300 ring-1 ring-inset ring-emerald-400/20">
              <span className="h-1.5 w-1.5 rounded-full bg-emerald-400 animate-pulse" />
              Charging
            </span>
          )}
          {session.location ? (
            <>
              <MapPin className="h-3.5 w-3.5 shrink-0 text-emerald-300/80" />
              <span className="truncate">{session.location}</span>
            </>
          ) : (
            <span className="truncate">{formatDate(start)}</span>
          )}
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-slate-500">
          {session.location && <span>{formatDate(start)}</span>}
          {session.location && <span>·</span>}
          <span>{formatTime(start)}</span>
          <span>·</span>
          <span>{fmtDuration(session.durationSecs)}</span>
          {active && livePowerKw != null && (
            <>
              <span>·</span>
              <span className="text-emerald-300">{livePowerKw} kW</span>
            </>
          )}
        </div>
        {/* Mobile: energy + SoC + cost sit below the meta line so the
            title gets the full row width instead of being squeezed. */}
        <div className="mt-1.5 flex items-center gap-2.5 tabular-nums sm:hidden">
          <span className="inline-flex items-center gap-1 text-sm font-semibold text-emerald-300">
            <Zap className="h-3.5 w-3.5" />
            {fmtEnergy(session.energyAddedKwh)}
          </span>
          <span className="text-xs text-slate-500">{socShort}</span>
          {session.cost != null && (
            <span className="text-xs font-medium text-slate-300">
              {fmtMoney(session.cost, session.currency)}
            </span>
          )}
        </div>
      </div>

      {/* Desktop: energy + SoC + cost as a right-aligned column. */}
      <div className="hidden shrink-0 text-right sm:block">
        <div className="flex items-center justify-end gap-1 text-sm font-semibold text-emerald-300 tabular-nums">
          <Zap className="h-3.5 w-3.5" />
          {fmtEnergy(session.energyAddedKwh)}
        </div>
        <div className="mt-0.5 text-xs text-slate-500 tabular-nums">{socFull}</div>
        {session.cost != null && (
          <div className="mt-0.5 text-xs font-medium text-slate-300 tabular-nums">
            {fmtMoney(session.cost, session.currency)}
          </div>
        )}
      </div>

      <div onClick={(e) => e.preventDefault()}>
        <TagPopover
          tags={session.tags}
          onChange={(t) => onTagsChange(session.id, t)}
        />
      </div>

      {session.locationLat != null && session.locationLon != null && (
        <MiniPinMap
          lat={session.locationLat}
          lon={session.locationLon}
          className="h-14 w-20 sm:h-20 sm:w-32"
        />
      )}

      <ChevronRight className="h-4 w-4 shrink-0 text-slate-600 transition-colors group-hover:text-slate-400" />
    </Link>
  )
}

function formatDate(d: Date): string {
  return d.toLocaleDateString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
  })
}

function formatTime(d: Date): string {
  return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}
