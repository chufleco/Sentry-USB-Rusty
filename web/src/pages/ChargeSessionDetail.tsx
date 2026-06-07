import { useCallback, useEffect, useState } from "react"
import { Link, useParams } from "react-router-dom"
import {
  ArrowLeft,
  BatteryCharging,
  DollarSign,
  Gauge,
  Leaf,
  Loader2,
  MapPin,
  Plug,
  Zap,
} from "lucide-react"
import { fetchChargeSession, setChargeTags } from "@/api/charging"
import type { ChargeSessionDetail } from "@/types/charging"
import { SectionHeading, StatTile } from "@/components/drives/StatTile"
import { TagPopover } from "@/components/drives/TagPopover"
import ChargePowerChart from "@/components/charging/ChargePowerChart"
import ChargingLineChart from "@/components/charging/ChargingLineChart"
import { MiniPinMap } from "@/components/charging/MiniPinMap"
import {
  fmtDuration,
  fmtEnergy,
  fmtMoney,
  fmtPercent,
  fmtPower,
  fmtRangeUnit,
  fmtSoc,
} from "@/lib/charge-format"
import { useDistanceUnit } from "@/hooks/useDistanceUnit"

export default function ChargeSessionDetailPage() {
  const { id } = useParams<{ id: string }>()
  const [session, setSession] = useState<ChargeSessionDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const metric = useDistanceUnit()

  useEffect(() => {
    if (!id) return
    let cancelled = false
    setLoading(true)
    setError(null)
    fetchChargeSession(id)
      .then((s) => {
        if (!cancelled) setSession(s)
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
  }, [id])

  const onTagsChange = useCallback(
    async (next: string[]) => {
      if (!id) return
      // Optimistic, then refetch so cost reflects the new tags' rate.
      setSession((prev) => (prev ? { ...prev, tags: next } : prev))
      try {
        await setChargeTags(id, next)
      } finally {
        try {
          setSession(await fetchChargeSession(id))
        } catch {
          /* keep optimistic tags if the refetch fails */
        }
      }
    },
    [id],
  )

  const rangeAdded =
    session?.startRangeMi != null && session?.endRangeMi != null
      ? session.endRangeMi - session.startRangeMi
      : null

  const hasCurrent = (session?.points ?? []).some((p) => p.currentA != null)
  const hasVoltage = (session?.points ?? []).some((p) => p.voltageV != null)
  const hasRange = (session?.points ?? []).some((p) => p.rangeMi != null)

  // The Range chart plots rangeMi directly; convert the series to km for
  // metric so the axis and tooltip match the unit.
  const rangePoints = metric
    ? (session?.points ?? []).map((p) => ({
        ...p,
        rangeMi: p.rangeMi == null ? null : p.rangeMi * 1.609344,
      }))
    : (session?.points ?? [])

  return (
    <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <Link
        to="/charging"
        className="mb-4 inline-flex items-center gap-1.5 text-sm text-slate-400 hover:text-slate-200"
      >
        <ArrowLeft className="h-4 w-4" />
        Charging
      </Link>

      {loading && (
        <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
          <Loader2 className="h-4 w-4 animate-spin" />
          Loading session…
        </div>
      )}
      {error && !loading && (
        <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
          Failed to load session: {error}
        </div>
      )}

      {!loading && !error && session && (
        <div className="space-y-6">
          <div className="flex items-start justify-between gap-3">
            <div>
              <h1 className="text-2xl font-semibold text-slate-100">
                {formatDateTime(session.startMs)}
              </h1>
              <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-slate-500">
                <span>&rarr; {formatEndLabel(session.startMs, session.endMs)}</span>
                <span>{fmtDuration(session.durationSecs)}</span>
                {session.location && (
                  <span className="inline-flex items-center gap-1">
                    <MapPin className="h-3.5 w-3.5" />
                    {session.location}
                  </span>
                )}
              </div>
            </div>
            <TagPopover tags={session.tags} onChange={onTagsChange} />
          </div>

          {session.locationLat != null && session.locationLon != null && (
            <MiniPinMap
              lat={session.locationLat}
              lon={session.locationLon}
              zoom={16}
              className="h-44 w-full"
            />
          )}

          <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
            <SectionHeading>Session</SectionHeading>
            <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
              <StatTile
                label="Energy added"
                value={fmtEnergy(session.energyAddedKwh)}
                icon={<BatteryCharging className="h-4 w-4" />}
                info="Energy added to the battery this session (reported by the car)."
              />
              <StatTile
                label="Energy used"
                value={fmtEnergy(session.energyUsedKwh)}
                icon={<Zap className="h-4 w-4" />}
                info="Energy drawn from the charger (wall-side), estimated by integrating charging power. Higher than energy added — the difference is charging loss."
              />
              <StatTile
                label="Efficiency"
                value={fmtPercent(session.efficiencyPct)}
                icon={<Leaf className="h-4 w-4" />}
                info="Energy added to the battery divided by energy drawn from the charger."
              />
              <StatTile
                label="Cost"
                value={
                  session.cost != null
                    ? fmtMoney(session.cost, session.currency)
                    : "—"
                }
                icon={<DollarSign className="h-4 w-4" />}
                info={
                  session.rate != null
                    ? `Charged on energy used at ${fmtMoney(session.rate, session.currency)}/kWh. Set rates from the Charging page.`
                    : "Set an electricity rate from the Charging page to see cost."
                }
              />
              <StatTile
                label="Peak power"
                value={fmtPower(session.peakPowerKw)}
                icon={<Zap className="h-4 w-4" />}
                info={
                  session.avgPowerKw != null
                    ? `Highest charging power seen this session. Average ${Math.round(session.avgPowerKw)} kW.`
                    : "Highest charging power seen this session."
                }
              />
              <StatTile
                label="Battery"
                value={
                  session.startSoc != null && session.endSoc != null
                    ? `${fmtSoc(session.startSoc)} → ${fmtSoc(session.endSoc)}`
                    : fmtSoc(session.endSoc)
                }
                icon={<BatteryCharging className="h-4 w-4" />}
                info="State of charge at start and end."
              />
              <StatTile
                label="Range added"
                value={rangeAdded != null ? fmtRangeUnit(rangeAdded, metric) : "—"}
                icon={<Gauge className="h-4 w-4" />}
                info="Rated range gained this session."
              />
              <StatTile
                label="Peak current"
                value={session.peakCurrentA != null ? `${session.peakCurrentA} A` : "—"}
                icon={<Plug className="h-4 w-4" />}
              />
              <StatTile
                label="Voltage"
                value={session.peakVoltageV != null ? `${session.peakVoltageV} V` : "—"}
                icon={<Zap className="h-4 w-4" />}
              />
              <StatTile
                label="Charge limit"
                value={fmtSoc(session.chargeLimitSoc)}
                icon={<BatteryCharging className="h-4 w-4" />}
                info="Target state of charge for this session."
              />
            </div>
          </div>

          {session.points.length > 1 && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Power &amp; battery</SectionHeading>
              <ChargePowerChart points={session.points} />
            </div>
          )}

          {session.points.length > 1 && hasRange && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Range</SectionHeading>
              <ChargingLineChart
                points={rangePoints}
                series={[{ key: "rangeMi", name: "Range", color: "#a78bfa" }]}
                unit={metric ? " km" : " mi"}
              />
            </div>
          )}

          {session.points.length > 1 && hasCurrent && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Amperage</SectionHeading>
              <ChargingLineChart
                points={session.points}
                series={[{ key: "currentA", name: "Current", color: "#fbbf24" }]}
                unit=" A"
              />
            </div>
          )}

          {session.points.length > 1 && hasVoltage && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Voltage</SectionHeading>
              <ChargingLineChart
                points={session.points}
                series={[{ key: "voltageV", name: "Voltage", color: "#22d3ee" }]}
                unit=" V"
                yDomain={[0, "dataMax + 10"]}
              />
            </div>
          )}

        </div>
      )}
    </div>
  )
}

function formatDateTime(ms: number): string {
  const d = new Date(ms)
  return d.toLocaleString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  })
}

// End of the session. Time-only when it ended the same day as it started
// (the common case); otherwise the full date too.
function formatEndLabel(startMs: number, endMs: number): string {
  const start = new Date(startMs)
  const end = new Date(endMs)
  const sameDay = start.toDateString() === end.toDateString()
  return end.toLocaleString(
    [],
    sameDay
      ? { hour: "numeric", minute: "2-digit" }
      : {
          weekday: "short",
          month: "short",
          day: "numeric",
          hour: "numeric",
          minute: "2-digit",
        },
  )
}
