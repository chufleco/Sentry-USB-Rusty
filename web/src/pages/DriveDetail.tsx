import { Suspense, lazy, useEffect, useMemo } from "react"
import { Link, useParams } from "react-router-dom"
import {
  ArrowLeft,
  BatteryFull,
  Camera,
  Clock,
  Disc,
  Gauge,
  Loader2,
  MapPin,
  Sparkles,
  Thermometer,
  Wind,
} from "lucide-react"
import { useDriveDetail } from "@/hooks/useDriveDetail"
import { ScrubberProvider, useScrubberSync } from "@/hooks/useScrubberSync"
import {
  formatDistance,
  formatDuration,
  formatHvacRuntime,
  formatPercent,
  formatPsi,
  formatSpeed,
  formatTempC,
} from "@/lib/drive-format"
import type { DriveDetail as DriveDetailType } from "@/types/drives"
import { DriveMap } from "@/components/drives/DriveMap"
import { DriveScrubber } from "@/components/drives/DriveScrubber"
import { DualPinBlock } from "@/components/drives/DualPinBlock"
import { FsdEngagementStripe } from "@/components/drives/FsdEngagementStripe"
import { SectionHeading, StatTile } from "@/components/drives/StatTile"
import { TagPopover } from "@/components/drives/TagPopover"

const DriveChart = lazy(() => import("@/components/drives/DriveChart"))

export default function DriveDetail() {
  const { id } = useParams<{ id: string }>()
  const { drive, loading, error, saveTags } = useDriveDetail(id)

  return (
    <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <Link
        to="/drives"
        className="mb-4 inline-flex items-center gap-2 text-sm text-slate-400 hover:text-slate-200"
      >
        <ArrowLeft className="h-4 w-4" />
        Back to drives
      </Link>

      {loading && (
        <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
          <Loader2 className="h-4 w-4 animate-spin" /> Loading drive…
        </div>
      )}
      {error && !loading && (
        <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
          Failed to load drive: {error}
        </div>
      )}
      {drive && (
        <ScrubberProvider>
          <DriveDetailContent drive={drive} onSaveTags={saveTags} />
        </ScrubberProvider>
      )}
    </div>
  )
}

interface DriveDetailContentProps {
  drive: DriveDetailType
  onSaveTags: (tags: string[]) => Promise<void>
}

function DriveDetailContent({ drive, onSaveTags }: DriveDetailContentProps) {
  const scrubber = useScrubberSync()
  const metric = false

  useEffect(() => {
    scrubber.setTotal(drive.points.length)
  }, [drive.points.length, scrubber])

  const title = drive.endLocation ?? "Drive"
  const sourceBadge = drive.source === "tessie" ? "Tessie" : "USB"
  const sourceClass =
    drive.source === "tessie"
      ? "bg-violet-500/15 text-violet-200 ring-violet-400/30"
      : "bg-emerald-500/15 text-emerald-200 ring-emerald-400/30"

  const speedSeries = useMemo(() => {
    return drive.points.map((p, i) => ({
      index: i,
      time: p[2],
      value: metric ? p[3] * 3.6 : p[3] * 2.23694,
    }))
  }, [drive.points, metric])

  const speedUnit = metric ? "km/h" : "mph"
  const fsdFull = drive.fsdPercent >= 100

  return (
    <>
      <div className="flex items-start justify-between gap-3">
        <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">
          Drive to {title}
        </h1>
        <span
          className={`mt-1 inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-xs font-medium ring-1 ring-inset ${sourceClass}`}
        >
          {sourceBadge}
        </span>
      </div>

      <div className="mt-4">
        <DriveMap
          points={drive.points}
          fsdEvents={drive.fsdEvents}
          source={drive.source}
        />
        <DriveScrubber points={drive.points} startTime={drive.startTime} />
        {drive.fsdStates && drive.fsdStates.length > 0 && (
          <FsdEngagementStripe fsdStates={drive.fsdStates} />
        )}
      </div>

      <div className="mt-6">
        <DualPinBlock
          origin={{
            label: drive.startLocation ?? "Unknown origin",
            batteryPct: drive.batteryPctStart,
            timestamp: drive.startTime,
          }}
          destination={{
            label: drive.endLocation ?? "Unknown destination",
            batteryPct: drive.batteryPctEnd,
            timestamp: drive.endTime,
          }}
          size="detail"
        />
      </div>

      <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-3">
        <StatTile
          label="Distance"
          value={formatDistance(drive.distanceMi, drive.distanceKm, metric)}
          icon={<Gauge className="h-4 w-4" />}
          size="headline"
        />
        <StatTile
          label="Self-driving"
          value={`${formatPercent(drive.fsdPercent)}%`}
          icon={<Sparkles className="h-4 w-4" />}
          star={fsdFull}
          info="Percentage of the drive's distance with FSD engaged."
          size="headline"
        />
        <StatTile
          label="Duration"
          value={formatDuration(drive.durationMs)}
          icon={<Clock className="h-4 w-4" />}
          size="headline"
        />
      </div>

      <SpeedSection
        drive={drive}
        speedSeries={speedSeries}
        metric={metric}
        speedUnit={speedUnit}
      />
      <AssistedSection drive={drive} metric={metric} />
      <OdometerSection drive={drive} />
      <BatterySection drive={drive} />
      <ClimateSection drive={drive} metric={metric} />
      <TirePressureSection drive={drive} />
      <DashcamSection drive={drive} />

      <SectionHeading>Tags</SectionHeading>
      <div className="flex flex-wrap items-center gap-2">
        {(drive.tags ?? []).length === 0 && (
          <span className="text-sm text-slate-500">No tags</span>
        )}
        {(drive.tags ?? []).map((t) => (
          <span
            key={t}
            className="rounded-full bg-emerald-400/10 px-2.5 py-0.5 text-xs text-emerald-200"
          >
            {t}
          </span>
        ))}
        <TagPopover tags={drive.tags ?? []} onChange={onSaveTags} />
      </div>
    </>
  )
}

interface SpeedSectionProps {
  drive: DriveDetailType
  speedSeries: { index: number; time: number; value: number }[]
  metric: boolean
  speedUnit: string
}

function SpeedSection({ drive, speedSeries, metric, speedUnit }: SpeedSectionProps) {
  if (drive.maxSpeedMph === 0 && drive.avgSpeedMph === 0) return null
  return (
    <>
      <SectionHeading>Speed</SectionHeading>
      <div className="grid grid-cols-2 gap-4">
        <StatTile
          label="Avg speed"
          value={formatSpeed(drive.avgSpeedMph, drive.avgSpeedKmh, metric)}
          icon={<Gauge className="h-4 w-4" />}
        />
        <StatTile
          label="Max speed"
          value={formatSpeed(drive.maxSpeedMph, drive.maxSpeedKmh, metric)}
          icon={<Gauge className="h-4 w-4" />}
        />
      </div>
      {speedSeries.length > 1 && (
        <div className="mt-4 rounded-2xl border border-white/[0.06] bg-white/[0.02] p-3">
          <Suspense fallback={<ChartFallback />}>
            <DriveChart
              series={speedSeries}
              valueLabel="Speed"
              valueFormatter={(n) => `${Math.round(n)} ${speedUnit}`}
              startTime={drive.startTime}
            />
          </Suspense>
        </div>
      )}
    </>
  )
}

function ChartFallback() {
  return (
    <div className="flex h-44 items-center justify-center text-xs text-slate-500">
      <Loader2 className="mr-2 h-3 w-3 animate-spin" /> Loading chart…
    </div>
  )
}

interface AssistedSectionProps {
  drive: DriveDetailType
  metric: boolean
}

function AssistedSection({ drive, metric }: AssistedSectionProps) {
  const hasAny =
    drive.fsdPercent > 0 ||
    drive.autosteerPercent > 0 ||
    drive.taccPercent > 0 ||
    drive.fsdDisengagements > 0 ||
    drive.fsdAccelPushes > 0
  if (!hasAny) return null
  return (
    <>
      <SectionHeading>Assisted driving</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        <StatTile
          label="FSD"
          value={`${formatPercent(drive.fsdPercent)}%`}
          icon={<Sparkles className="h-4 w-4" />}
          info="Time + distance share with Full Self-Driving (Supervised) engaged."
        />
        <StatTile
          label="FSD distance"
          value={formatDistance(drive.fsdDistanceMi, drive.fsdDistanceKm, metric)}
          icon={<Gauge className="h-4 w-4" />}
        />
        <StatTile
          label="Disengagements"
          value={String(drive.fsdDisengagements)}
          icon={<Disc className="h-4 w-4" />}
          info="Number of times FSD handed back control (excluding parks within 2s)."
        />
        <StatTile
          label="Accel pushes"
          value={String(drive.fsdAccelPushes)}
          icon={<Gauge className="h-4 w-4" />}
          info="Number of accelerator presses while FSD was engaged."
        />
        <StatTile
          label="Autopilot"
          value={`${formatPercent(drive.autosteerPercent)}%`}
          icon={<Sparkles className="h-4 w-4" />}
          info="Autosteer share (lane-keeping without FSD)."
        />
        <StatTile
          label="Autopilot distance"
          value={formatDistance(drive.autosteerDistanceMi, drive.autosteerDistanceKm, metric)}
          icon={<Gauge className="h-4 w-4" />}
        />
        <StatTile
          label="TACC"
          value={`${formatPercent(drive.taccPercent)}%`}
          icon={<Sparkles className="h-4 w-4" />}
          info="Traffic-Aware Cruise Control share (speed regulation only)."
        />
        <StatTile
          label="TACC distance"
          value={formatDistance(drive.taccDistanceMi, drive.taccDistanceKm, metric)}
          icon={<Gauge className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

function OdometerSection({ drive }: { drive: DriveDetailType }) {
  if (drive.odometerMiStart === undefined && drive.odometerMiEnd === undefined) return null
  return (
    <>
      <SectionHeading>Odometer</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatTile
          label="Start"
          value={drive.odometerMiStart !== undefined ? `${drive.odometerMiStart.toFixed(1)} mi` : "—"}
          icon={<MapPin className="h-4 w-4" />}
        />
        <StatTile
          label="End"
          value={drive.odometerMiEnd !== undefined ? `${drive.odometerMiEnd.toFixed(1)} mi` : "—"}
          icon={<MapPin className="h-4 w-4" />}
        />
        <StatTile
          label="Driven"
          value={drive.odometerMiDriven !== undefined ? `${drive.odometerMiDriven.toFixed(1)} mi` : "—"}
          icon={<Gauge className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

function BatterySection({ drive }: { drive: DriveDetailType }) {
  if (drive.batteryPctStart === undefined && drive.batteryPctEnd === undefined) return null
  return (
    <>
      <SectionHeading>Battery</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatTile
          label="Start"
          value={drive.batteryPctStart !== undefined ? `${Math.round(drive.batteryPctStart)}%` : "—"}
          icon={<BatteryFull className="h-4 w-4" />}
        />
        <StatTile
          label="End"
          value={drive.batteryPctEnd !== undefined ? `${Math.round(drive.batteryPctEnd)}%` : "—"}
          icon={<BatteryFull className="h-4 w-4" />}
        />
        <StatTile
          label="Used"
          value={drive.batteryPctUsed !== undefined ? `${drive.batteryPctUsed.toFixed(1)}%` : "—"}
          icon={<BatteryFull className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

interface ClimateSectionProps {
  drive: DriveDetailType
  metric: boolean
}

function ClimateSection({ drive, metric }: ClimateSectionProps) {
  const anyClimate =
    drive.interiorTempMinC !== undefined ||
    drive.interiorTempMaxC !== undefined ||
    drive.exteriorTempAvgC !== undefined ||
    drive.hvacRuntimeS !== undefined
  if (!anyClimate) return null
  return (
    <>
      <SectionHeading>Climate</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        <StatTile
          label="Interior min"
          value={formatTempC(drive.interiorTempMinC, metric)}
          icon={<Thermometer className="h-4 w-4" />}
        />
        <StatTile
          label="Interior max"
          value={formatTempC(drive.interiorTempMaxC, metric)}
          icon={<Thermometer className="h-4 w-4" />}
        />
        <StatTile
          label="Exterior avg"
          value={formatTempC(drive.exteriorTempAvgC, metric)}
          icon={<Thermometer className="h-4 w-4" />}
        />
        <StatTile
          label="HVAC runtime"
          value={drive.hvacRuntimeS !== undefined ? formatHvacRuntime(drive.hvacRuntimeS) : "—"}
          icon={<Wind className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

function TirePressureSection({ drive }: { drive: DriveDetailType }) {
  const any =
    drive.tireFlPsi !== undefined ||
    drive.tireFrPsi !== undefined ||
    drive.tireRlPsi !== undefined ||
    drive.tireRrPsi !== undefined
  if (!any) return null
  return (
    <>
      <SectionHeading>Tire pressure</SectionHeading>
      <div className="grid grid-cols-2 gap-4">
        <StatTile label="FL" value={formatPsi(drive.tireFlPsi)} icon={<Disc className="h-4 w-4" />} />
        <StatTile label="FR" value={formatPsi(drive.tireFrPsi)} icon={<Disc className="h-4 w-4" />} />
        <StatTile label="RL" value={formatPsi(drive.tireRlPsi)} icon={<Disc className="h-4 w-4" />} />
        <StatTile label="RR" value={formatPsi(drive.tireRrPsi)} icon={<Disc className="h-4 w-4" />} />
      </div>
    </>
  )
}

function DashcamSection({ drive }: { drive: DriveDetailType }) {
  if (!drive.clipCount) return null
  return (
    <>
      <SectionHeading>Dashcam</SectionHeading>
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
        <StatTile
          label="Clips"
          value={String(drive.clipCount)}
          icon={<Camera className="h-4 w-4" />}
        />
      </div>
    </>
  )
}
