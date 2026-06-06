import { useCallback, useEffect, useState } from "react"
import { Link } from "react-router-dom"
import {
  BatteryCharging,
  ChevronRight,
  Loader2,
  MapPin,
  RefreshCw,
  Zap,
} from "lucide-react"
import { fetchChargeSessions } from "@/api/charging"
import type { ChargeSessionSummary } from "@/types/charging"
import { fmtDuration, fmtEnergy, fmtSoc } from "@/lib/charge-format"

export default function Charging() {
  const [sessions, setSessions] = useState<ChargeSessionSummary[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    setLoading(true)
    setError(null)
    fetchChargeSessions()
      .then((s) => setSessions(s))
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false))
  }, [])

  useEffect(() => {
    load()
  }, [load])

  const totalEnergy = sessions.reduce(
    (sum, s) => sum + (s.energyAddedKwh ?? 0),
    0,
  )

  return (
    <div className="mx-auto w-full max-w-5xl px-4 py-6 sm:px-6 sm:py-8">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3 sm:mb-6">
        <div>
          <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">
            Charging
          </h1>
          {sessions.length > 0 && (
            <p className="mt-1 text-sm text-slate-500">
              {sessions.length} session{sessions.length === 1 ? "" : "s"} ·{" "}
              {fmtEnergy(totalEnergy)} added
            </p>
          )}
        </div>
        <button
          type="button"
          onClick={load}
          className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-3 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06]"
        >
          <RefreshCw className="h-3.5 w-3.5" />
          Refresh
        </button>
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
        {!loading && !error && sessions.length === 0 && (
          <div className="rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-center text-sm text-slate-400">
            <BatteryCharging className="mx-auto mb-3 h-8 w-8 text-slate-600" />
            No charging sessions recorded yet. Sessions appear here once the
            car charges while the Pi is sampling.
          </div>
        )}
        {!loading &&
          sessions.map((s) => <ChargeRow key={s.id} session={s} />)}
      </div>
    </div>
  )
}

function ChargeRow({ session }: { session: ChargeSessionSummary }) {
  const start = new Date(session.startMs)
  // "19% (40 mi) → 90% (193 mi)" when range is known, else just the
  // SoC pair, matching how TeslaScope labels a session.
  const socPart = (
    pct: number | null,
    mi: number | null,
  ): string => {
    if (pct == null) return "—"
    return mi != null ? `${fmtSoc(pct)} (${Math.round(mi)} mi)` : fmtSoc(pct)
  }
  const socDelta =
    session.startSoc != null && session.endSoc != null
      ? `${socPart(session.startSoc, session.startRangeMi)} → ${socPart(session.endSoc, session.endRangeMi)}`
      : session.endSoc != null
        ? socPart(session.endSoc, session.endRangeMi)
        : "—"

  return (
    <Link
      to={`/charging/${session.id}`}
      className="group flex items-center gap-4 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-4 transition-colors hover:border-white/10 hover:bg-white/[0.04]"
    >
      <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-300 ring-1 ring-inset ring-emerald-500/20">
        <BatteryCharging className="h-5 w-5" />
      </span>

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm font-medium text-slate-100">
          {formatDate(start)}
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-slate-500">
          <span>{formatTime(start)}</span>
          <span>·</span>
          <span>{fmtDuration(session.durationSecs)}</span>
          {session.location && (
            <>
              <span>·</span>
              <span className="inline-flex items-center gap-1 truncate">
                <MapPin className="h-3 w-3 shrink-0" />
                <span className="truncate">{session.location}</span>
              </span>
            </>
          )}
        </div>
      </div>

      <div className="hidden shrink-0 text-right sm:block">
        <div className="flex items-center justify-end gap-1 text-sm font-semibold text-emerald-300 tabular-nums">
          <Zap className="h-3.5 w-3.5" />
          {fmtEnergy(session.energyAddedKwh)}
        </div>
        <div className="mt-0.5 text-xs text-slate-500 tabular-nums">{socDelta}</div>
      </div>

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
