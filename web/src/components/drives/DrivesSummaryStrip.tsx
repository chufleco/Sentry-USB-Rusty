import { Activity, Clock, Gauge, MapPin, Sparkles } from "lucide-react"
import {
  formatDuration,
  formatMiles,
  formatPercent,
} from "@/lib/drive-format"
import type {
  DatePreset,
  DateRange,
  DrivesFilteredStats,
} from "@/hooks/useDrivesList"

const PRESET_LABELS: Record<DatePreset, string> = {
  today: "Today",
  yesterday: "Yesterday",
  last7: "Last 7 days",
  last30: "Last 30 days",
  thisYear: "This year",
  lastYear: "Last year",
  all: "All time",
}

function rangeLabel(range: DateRange): string {
  if (range.kind === "custom") {
    return `${range.start} – ${range.end}`
  }
  return PRESET_LABELS[range.preset] ?? "Last 7 days"
}

interface DrivesSummaryStripProps {
  stats: DrivesFilteredStats
  range: DateRange
  loading: boolean
}

/**
 * Compact lifetime-of-current-selection stats strip displayed near
 * the top of the Drives list. Mirrors what the old UI showed in the
 * page header (drives count, total distance, total duration, FSD %,
 * autopilot %) but recomputes against whatever the user currently
 * has filtered — switching the date preset from "Last 7 days" to
 * "Last 30 days" updates these numbers live.
 */
export function DrivesSummaryStrip({
  stats,
  range,
  loading,
}: DrivesSummaryStripProps) {
  // While the initial fetch is in flight render a skeleton row so the
  // header doesn't pop in. On subsequent refreshes (refresh after
  // process/import) we keep showing the previous numbers — that's
  // smoother than flashing back to a skeleton.
  if (loading && stats.count === 0) {
    return (
      <div className="flex flex-wrap items-center gap-x-6 gap-y-3 rounded-2xl border border-white/[0.06] bg-white/[0.025] px-5 py-3.5">
        <div className="h-9 w-24 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-9 w-28 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-9 w-24 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-9 w-20 animate-pulse rounded-md bg-white/[0.04]" />
      </div>
    )
  }

  // Empty selection — render the strip with zeroes so the layout
  // stays stable. The label still clarifies which range produced them.
  const rangeText = rangeLabel(range)

  return (
    <div className="flex flex-wrap items-center gap-x-6 gap-y-3 rounded-2xl border border-white/[0.06] bg-white/[0.025] px-5 py-3.5">
      <RangeBadge label={rangeText} />
      <Divider />
      <StatCell
        icon={<MapPin className="h-3.5 w-3.5" />}
        label="Drives"
        value={stats.count.toLocaleString()}
      />
      <Divider />
      <StatCell
        icon={<Gauge className="h-3.5 w-3.5" />}
        label="Distance"
        value={formatMiles(stats.totalDistanceMi, 1)}
      />
      <Divider />
      <StatCell
        icon={<Clock className="h-3.5 w-3.5" />}
        label="Time"
        value={formatDuration(stats.totalDurationMs)}
      />
      {stats.fsdEngagedMs > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Sparkles className="h-3.5 w-3.5 text-emerald-300" />}
            label="FSD"
            value={`${formatPercent(stats.fsdPercent)}%`}
            highlight={stats.fsdPercent >= 99}
          />
        </>
      )}
      {stats.autopilotEngagedMs > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Activity className="h-3.5 w-3.5" />}
            label="Autopilot"
            value={`${formatPercent(stats.autopilotPercent)}%`}
          />
        </>
      )}
      {stats.fsdDisengagements > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Sparkles className="h-3.5 w-3.5 text-rose-300" />}
            label="Disengagements"
            value={stats.fsdDisengagements.toLocaleString()}
          />
        </>
      )}
      {stats.tessieCount > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Sparkles className="h-3.5 w-3.5 text-violet-300" />}
            label="Tessie"
            value={stats.tessieCount.toLocaleString()}
          />
        </>
      )}
    </div>
  )
}

interface StatCellProps {
  icon: React.ReactNode
  label: string
  value: React.ReactNode
  highlight?: boolean
}

function StatCell({ icon, label, value, highlight }: StatCellProps) {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span
        className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-white/[0.04] ring-1 ring-inset ring-white/10 text-slate-300"
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
          {label}
        </div>
        <div
          className={
            "mt-0.5 text-sm font-semibold tabular-nums " +
            (highlight ? "text-emerald-300" : "text-slate-100")
          }
        >
          {value}
        </div>
      </div>
    </div>
  )
}

function RangeBadge({ label }: { label: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
        Showing
      </span>
      <span className="rounded-full border border-emerald-400/30 bg-emerald-500/10 px-2 py-0.5 text-[11px] font-medium text-emerald-200">
        {label}
      </span>
    </div>
  )
}

function Divider() {
  return <span aria-hidden className="hidden h-8 w-px bg-white/[0.06] sm:block" />
}
