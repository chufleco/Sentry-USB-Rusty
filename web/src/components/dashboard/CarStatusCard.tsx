import { Suspense, lazy, useEffect, useMemo, useState } from "react"
import { Link } from "react-router-dom"
import {
  BatteryCharging,
  BatteryMedium,
  Car,
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Disc,
  Music2,
  Thermometer,
} from "lucide-react"
import type { TireHistoryResponse } from "./TirePressureCard"
import type { CurrentCharge } from "@/types/charging"
import { fmtRangeUnit, fmtToFull } from "@/lib/charge-format"

// Lazy-load the chart only when the user expands the Tires chip —
// recharts (380 KB) stays out of the dashboard's initial bundle for
// users who only glance at the summary.
const TirePressureCard = lazy(() =>
  import("./TirePressureCard").then((m) => ({ default: m.TirePressureCard })),
)

export interface CarStatusSample {
  ts: number | null
  battery_pct?: number | null
  interior_temp_c?: number | null
  exterior_temp_c?: number | null
  tire_fl_psi?: number | null
  tire_fr_psi?: number | null
  tire_rl_psi?: number | null
  tire_rr_psi?: number | null
}

interface CarStatusCardProps {
  sample: CarStatusSample | null
  // ISO end-time of the most recent drive — used to derive
  // "Parked Xh Ym". When the value is null the duration row is
  // hidden (no drives recorded yet).
  latestDriveEnd: string | null
  // Tire history for the expandable chart. Pass undefined to hide
  // the Tires chip's expand affordance entirely (e.g. no telemetry).
  tireHistory?: TireHistoryResponse
  useFahrenheit: boolean
  // Distance unit for the battery drop-down's range row (true = km).
  metric: boolean
  // Live charge status. When the car is charging the Battery chip turns
  // green and pulses; expanding it shows range, time-to-full and power.
  // null/undefined hides the chip's expand affordance.
  currentCharge?: CurrentCharge | null
  // Name of the currently-active lock-chime sound, if the feature
  // is configured. null/undefined hides the indicator entirely so
  // users who don't use lock chimes don't see a confusing chip.
  lockChimeName?: string | null
}

type TireStatus =
  | { kind: "optimal"; label: string; color: string }
  | { kind: "check"; label: string; color: string }
  | { kind: "unsafe"; label: string; color: string }
  | { kind: "none"; label: string; color: string }

function deriveTireStatus(sample: CarStatusSample | null): TireStatus {
  if (!sample) return { kind: "none", label: "—", color: "text-slate-500" }
  const values = [
    sample.tire_fl_psi,
    sample.tire_fr_psi,
    sample.tire_rl_psi,
    sample.tire_rr_psi,
  ].filter((v): v is number => typeof v === "number")
  if (values.length === 0) {
    return { kind: "none", label: "—", color: "text-slate-500" }
  }
  // Mirrors the zone thresholds the chart uses: optimal 36–45,
  // warning bands 28–36 and 45–50, unsafe outside that.
  const anyUnsafe = values.some((v) => v < 28 || v > 50)
  if (anyUnsafe) {
    return { kind: "unsafe", label: "Unsafe", color: "text-rose-400" }
  }
  const anyWarn = values.some((v) => v < 36 || v > 45)
  if (anyWarn) {
    return { kind: "check", label: "Check tires", color: "text-amber-400" }
  }
  return { kind: "optimal", label: "Optimal", color: "text-emerald-400" }
}

function formatDuration(ms: number): string {
  const totalMin = Math.max(0, Math.floor(ms / 60_000))
  const d = Math.floor(totalMin / (60 * 24))
  const h = Math.floor((totalMin - d * 60 * 24) / 60)
  const m = totalMin - d * 24 * 60 - h * 60
  if (d > 0) return `${d}d ${h}h`
  if (h > 0) return `${h}h ${m}m`
  return `${m}m`
}

function formatTemp(c: number | null | undefined, useFahrenheit: boolean): string {
  if (c === null || c === undefined) return "—"
  const value = useFahrenheit ? (c * 9) / 5 + 32 : c
  const unit = useFahrenheit ? "°F" : "°C"
  return `${Math.round(value)}${unit}`
}

/**
 * Top-of-dashboard car-status overview. Replaces the old
 * stand-alone tire-pressure card with a single tile that shows the
 * last-known summary (parked duration, battery, cabin/ambient
 * temps, tire-health verdict) and reveals the tire-pressure history
 * chart inline when the user clicks the Tires chip.
 *
 * The chart bundle is lazy-loaded — clicking Tires is what pulls it
 * in, so users who never expand it pay zero recharts cost.
 */
export function CarStatusCard({
  sample,
  latestDriveEnd,
  tireHistory,
  useFahrenheit,
  metric,
  currentCharge,
  lockChimeName,
}: CarStatusCardProps) {
  const [tiresOpen, setTiresOpen] = useState(false)
  const [batteryOpen, setBatteryOpen] = useState(false)
  // Now tick — drives the parked-duration counter forward without
  // needing to re-render the whole dashboard. 1-minute cadence
  // matches the granularity of the displayed value ("5h 31m") so
  // updates aren't wasted. Date.now() lives in the state initialiser
  // and the interval body, never in render itself (React 19 rule).
  const [nowMs, setNowMs] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 60_000)
    return () => clearInterval(id)
  }, [])

  // Derived parked duration. We treat "latest drive ended in the
  // past" as the parked-since timestamp; if there's no recorded
  // drive yet we just show the state badge without a duration.
  const parkedDuration = useMemo(() => {
    if (!latestDriveEnd) return null
    const t = new Date(latestDriveEnd).getTime()
    if (!Number.isFinite(t)) return null
    const delta = nowMs - t
    if (delta < 60_000) return null
    return formatDuration(delta)
  }, [latestDriveEnd, nowMs])

  const tireStatus = deriveTireStatus(sample)
  const haveTireData =
    !!tireHistory && tireHistory.points.length > 0 && tireStatus.kind !== "none"

  const charging = !!currentCharge?.charging
  // Prefer the live charge SoC over the last BLE sample's battery_pct.
  const batterySoc = currentCharge?.soc ?? sample?.battery_pct
  const haveChargeDetail =
    currentCharge != null &&
    (currentCharge.charging || currentCharge.rangeMi != null)

  return (
    <div className="glass-card relative p-4">
      {/* Lock-chime chip pinned to the card's actual top-right
          corner via absolute positioning, so it sits in the corner
          regardless of the Parked row's height. Only renders when
          a chime is active so users without the feature don't see
          an empty placeholder. Click → /community?view=chimes
          which lands directly on the lock-chime tab inside
          Community (the LockChime page is mounted as a sub-view
          of Community, not its own route). */}
      {lockChimeName && (
        <Link
          to="/community?view=chimes"
          title={`Active lock chime: ${lockChimeName}`}
          className="absolute right-3 top-3 inline-flex max-w-[180px] items-center gap-1.5 rounded-full border border-emerald-400/25 bg-emerald-500/10 px-2.5 py-1 text-[11px] font-medium text-emerald-300 transition-colors hover:bg-emerald-500/15"
        >
          <Music2 className="h-3 w-3 shrink-0" />
          <span className="truncate">{lockChimeName}</span>
          <ChevronRight className="h-3 w-3 shrink-0 text-emerald-400/60" />
        </Link>
      )}

      {/* Top row — car state + duration. Right padding reserves room
          for the absolutely-positioned chime chip when present so
          long durations / labels can't slide under it. */}
      <div className={"flex items-center gap-3 " + (lockChimeName ? "pr-32 sm:pr-40" : "")}>
        <span className="tile-icon halo-accent">
          <Car className="h-4 w-4" />
        </span>
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold text-slate-100">Parked</div>
          {parkedDuration && (
            <div className="text-[11px] text-slate-500">{parkedDuration}</div>
          )}
        </div>
      </div>

      {/* Chip row — battery / interior / exterior / tires */}
      <div className="mt-4 flex flex-wrap items-stretch gap-3">
        <StatusChip
          icon={
            charging ? (
              <BatteryCharging className="h-3.5 w-3.5 animate-pulse" />
            ) : (
              <BatteryMedium className="h-3.5 w-3.5" />
            )
          }
          label={charging ? "Charging" : "Battery"}
          value={batterySoc != null ? `${Math.round(batterySoc)}%` : "—"}
          accent={charging}
          valueClass={charging ? "text-emerald-300" : undefined}
          onClick={haveChargeDetail ? () => setBatteryOpen((o) => !o) : undefined}
          trailing={
            haveChargeDetail ? (
              batteryOpen ? (
                <ChevronUp className="h-3.5 w-3.5 text-slate-500" />
              ) : (
                <ChevronDown className="h-3.5 w-3.5 text-slate-500" />
              )
            ) : null
          }
        />
        <StatusChip
          icon={<Thermometer className="h-3.5 w-3.5" />}
          label="Interior"
          value={formatTemp(sample?.interior_temp_c, useFahrenheit)}
        />
        <StatusChip
          icon={<Thermometer className="h-3.5 w-3.5" />}
          label="Exterior"
          value={formatTemp(sample?.exterior_temp_c, useFahrenheit)}
        />
        <StatusChip
          icon={<Disc className="h-3.5 w-3.5" />}
          label="Tires"
          value={tireStatus.label}
          valueClass={tireStatus.color}
          onClick={haveTireData ? () => setTiresOpen((o) => !o) : undefined}
          trailing={
            haveTireData ? (
              tiresOpen ? (
                <ChevronUp className="h-3.5 w-3.5 text-slate-500" />
              ) : (
                <ChevronDown className="h-3.5 w-3.5 text-slate-500" />
              )
            ) : null
          }
        />
      </div>

      {/* Battery drop-down — range / time-to-full / power, shown when the
          chip is expanded. Only the range row appears when idle. */}
      {batteryOpen && haveChargeDetail && currentCharge && (
        <div className="mt-4 border-t border-white/[0.06] pt-4">
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            <MiniStat label="Range" value={fmtRangeUnit(currentCharge.rangeMi, metric)} />
            {charging && (
              <MiniStat
                label="Time to full"
                value={fmtToFull(currentCharge.minutesToFull) ?? "—"}
              />
            )}
            {charging && currentCharge.powerKw != null && (
              <MiniStat label="Power" value={`${currentCharge.powerKw} kW`} />
            )}
            {charging && currentCharge.limitSoc != null && (
              <MiniStat label="Charge limit" value={`${currentCharge.limitSoc}%`} />
            )}
          </div>
          <Link
            to="/charging"
            className="mt-3 inline-flex items-center gap-1 text-xs font-medium text-emerald-300 hover:text-emerald-200"
          >
            View charging history
            <ChevronRight className="h-3.5 w-3.5" />
          </Link>
        </div>
      )}

      {/* Expandable chart — only mounts when the user clicks Tires.
          Lazy-loaded so users who don't expand never pull recharts. */}
      {tiresOpen && haveTireData && tireHistory && (
        <div className="mt-4 border-t border-white/[0.06] pt-4">
          <div className="mb-2 text-[11px] uppercase tracking-wider text-slate-500">
            Tire pressure · Last {tireHistory.days} days
          </div>
          <Suspense
            fallback={
              <div className="flex h-72 items-center justify-center text-sm text-slate-500">
                Loading tire history…
              </div>
            }
          >
            <TirePressureCard data={tireHistory} chartOnly />
          </Suspense>
        </div>
      )}
    </div>
  )
}

interface StatusChipProps {
  icon: React.ReactNode
  label: string
  value: string
  valueClass?: string
  // Green-tinted chip + icon ring, used for the charging state.
  accent?: boolean
  onClick?: () => void
  trailing?: React.ReactNode
}

function StatusChip({
  icon,
  label,
  value,
  valueClass,
  accent,
  onClick,
  trailing,
}: StatusChipProps) {
  const isButton = !!onClick
  const Wrapper = (isButton ? "button" : "div") as "button" | "div"
  return (
    <Wrapper
      {...(isButton ? { type: "button", onClick } : {})}
      className={
        "flex flex-1 min-w-[140px] items-center gap-2.5 rounded-xl border px-3 py-2 text-left transition-colors " +
        (accent
          ? "border-emerald-400/30 bg-emerald-500/10 "
          : "border-white/[0.06] bg-white/[0.025] ") +
        (isButton
          ? accent
            ? "hover:bg-emerald-500/15 cursor-pointer"
            : "hover:bg-white/[0.05] cursor-pointer"
          : "")
      }
    >
      <span
        className={
          "flex h-7 w-7 shrink-0 items-center justify-center rounded-full ring-1 ring-inset " +
          (accent
            ? "bg-emerald-500/15 ring-emerald-400/30 text-emerald-300"
            : "bg-white/[0.04] ring-white/10 text-slate-300")
        }
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0 flex-1">
        <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
          {label}
        </div>
        <div
          className={
            "mt-0.5 text-sm font-semibold tabular-nums leading-tight " +
            (valueClass ?? "text-slate-100")
          }
        >
          {value}
        </div>
      </div>
      {trailing}
    </Wrapper>
  )
}

function MiniStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
        {label}
      </div>
      <div className="mt-0.5 text-sm font-semibold tabular-nums leading-tight text-slate-100">
        {value}
      </div>
    </div>
  )
}
