import { useEffect, useState } from "react"
import { Link, useParams } from "react-router-dom"
import {
  ArrowLeft,
  BatteryCharging,
  Gauge,
  Loader2,
  MapPin,
  Plug,
  Zap,
} from "lucide-react"
import { fetchChargeSession } from "@/api/charging"
import type { ChargeSessionDetail } from "@/types/charging"
import { SectionHeading, StatTile } from "@/components/drives/StatTile"
import ChargePowerChart from "@/components/charging/ChargePowerChart"
import ChargingLineChart from "@/components/charging/ChargingLineChart"
import type { ChargePoint } from "@/types/charging"
import { fmtDuration, fmtEnergy, fmtPower, fmtRange, fmtSoc } from "@/lib/charge-format"

export default function ChargeSessionDetailPage() {
  const { id } = useParams<{ id: string }>()
  const [session, setSession] = useState<ChargeSessionDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

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

  const rangeAdded =
    session?.startRangeMi != null && session?.endRangeMi != null
      ? session.endRangeMi - session.startRangeMi
      : null

  // Battery temperature is stored in °C; charge sessions are shown in
  // °F to match the rest of the US-default UI. Map once so the chart's
  // axis, line and tooltip all speak °F. Nulls stay null → render gaps.
  const tempPoints: ChargePoint[] = (session?.points ?? []).map((p) => ({
    ...p,
    batteryTempC: p.batteryTempC == null ? null : (p.batteryTempC * 9) / 5 + 32,
  }))
  const hasTemp = tempPoints.some((p) => p.batteryTempC != null)
  const hasCurrent = (session?.points ?? []).some((p) => p.currentA != null)
  const hasVoltage = (session?.points ?? []).some((p) => p.voltageV != null)
  const hasRange = (session?.points ?? []).some((p) => p.rangeMi != null)

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
          <div>
            <h1 className="text-2xl font-semibold text-slate-100">
              {formatDateTime(session.startMs)}
            </h1>
            <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-slate-500">
              <span>{fmtDuration(session.durationSecs)}</span>
              {session.location && (
                <span className="inline-flex items-center gap-1">
                  <MapPin className="h-3.5 w-3.5" />
                  {session.location}
                </span>
              )}
            </div>
          </div>

          <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
            <SectionHeading>Session</SectionHeading>
            <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
              <StatTile
                label="Energy added"
                value={fmtEnergy(session.energyAddedKwh)}
                icon={<BatteryCharging className="h-4 w-4" />}
                info="Energy added across this charging session."
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
                value={rangeAdded != null ? fmtRange(rangeAdded) : "—"}
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
                points={session.points}
                series={[{ key: "rangeMi", name: "Range", color: "#a78bfa" }]}
                unit=" mi"
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

          {session.points.length > 1 && hasTemp && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Battery temperature</SectionHeading>
              <ChargingLineChart
                points={tempPoints}
                series={[{ key: "batteryTempC", name: "Battery temp", color: "#fb7185" }]}
                unit="°F"
                yDomain={["dataMin - 4", "dataMax + 4"]}
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
