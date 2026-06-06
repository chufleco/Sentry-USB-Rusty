import { BatteryCharging, Clock, Gauge, Zap } from "lucide-react"
import { fmtDuration } from "@/lib/charge-format"

export interface ChargingStats {
  count: number
  totalEnergyKwh: number
  totalDurationSecs: number
}

// Compact stats strip for the Charging page, mirroring the Drives
// summary strip: a few aggregate cells for the current filter set,
// recomputed live as the date filter changes. No session-count cell is
// duplicated in the pagination row, but charging has no pagination yet
// so the count stays here.
export function ChargingSummaryStrip({
  stats,
  loading,
}: {
  stats: ChargingStats
  loading: boolean
}) {
  if (loading && stats.count === 0) {
    return (
      <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
        <div className="h-8 w-24 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
      </div>
    )
  }

  const avgKwh = stats.count > 0 ? stats.totalEnergyKwh / stats.count : 0

  return (
    <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
      <StatCell
        icon={<BatteryCharging className="h-3.5 w-3.5" />}
        label="Sessions"
        value={stats.count.toLocaleString()}
      />
      <Divider />
      <StatCell
        icon={<Zap className="h-3.5 w-3.5 text-emerald-300" />}
        label="Energy added"
        value={`${stats.totalEnergyKwh.toFixed(1)} kWh`}
      />
      <Divider />
      <StatCell
        icon={<Clock className="h-3.5 w-3.5" />}
        label="Time charging"
        value={fmtDuration(stats.totalDurationSecs)}
      />
      {stats.count > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Gauge className="h-3.5 w-3.5" />}
            label="Avg / session"
            value={`${avgKwh.toFixed(1)} kWh`}
          />
        </>
      )}
    </div>
  )
}

function StatCell({
  icon,
  label,
  value,
}: {
  icon: React.ReactNode
  label: string
  value: React.ReactNode
}) {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-white/[0.04] ring-1 ring-inset ring-white/10 text-slate-300"
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
          {label}
        </div>
        <div className="text-sm font-semibold tabular-nums leading-tight text-slate-100">
          {value}
        </div>
      </div>
    </div>
  )
}

function Divider() {
  return <span aria-hidden className="hidden h-7 w-px bg-white/[0.06] sm:block" />
}
