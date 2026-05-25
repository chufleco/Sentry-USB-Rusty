import { useEffect, useRef, useState } from "react"
import { Link } from "react-router-dom"
import {
  Thermometer,
  HardDrive,
  Wifi,
  WifiOff,
  Clock,
  Camera,
  Activity,
  Cable,
  HeartPulse,
  Timer,
  Zap,
  ChevronRight,
  Download,
  AlertTriangle,
  Wind,
} from "lucide-react"
import { api } from "@/lib/api"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useAwayMode } from "@/hooks/useAwayMode"
import { useUpdateAvailable } from "@/hooks/useUpdateAvailable"
import type { PiStatus, DriveStats, StorageBreakdown } from "@/lib/api"
import { wsClient } from "@/lib/ws"
import { formatUptime, formatBytes, formatTemp } from "@/lib/utils"
import { CloudStatusBar } from "@/components/CloudStatusBar"
import { StatusTile, Row, TileDivider } from "@/components/ui/StatusTile"
import { BannerStack, type BannerItem } from "@/components/ui/Banner"
import { Pill, LiveDot } from "@/components/ui/Pill"
import type { Halo } from "@/components/ui/StatusTile"
import { TirePressureCard } from "@/components/dashboard/TirePressureCard"

function getTempHalo(milliC: number): Halo {
  if (milliC <= 0) return "blue"
  if (milliC < 55000) return "accent"
  if (milliC < 70000) return "amber"
  return "red"
}

function getTempColor(milliC: number): string {
  if (milliC < 55000) return "oklch(0.82 0.18 150)"
  if (milliC < 70000) return "#fbbf24"
  return "#f87171"
}

function getStorageHalo(usedPct: number): Halo {
  if (usedPct > 90) return "red"
  if (usedPct > 75) return "amber"
  return "accent"
}

function formatThroughput(bps: number): string {
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mbps`
  if (bps >= 1_000) return `${Math.round(bps / 1_000)} Kbps`
  return bps > 0 ? "< 1 Kbps" : "—"
}

function getWifiStrengthBars(strength: string): number {
  if (!strength) return 0
  const parts = strength.split("/")
  if (parts.length !== 2) return 0
  const ratio = parseInt(parts[0]) / parseInt(parts[1])
  if (ratio > 0.75) return 4
  if (ratio > 0.5) return 3
  if (ratio > 0.25) return 2
  return 1
}

// Mini 4-bar signal indicator. Filled bars get the tile's accent colour;
// the rest are a muted slate so the gauge reads at a glance.
function WifiBars({ bars }: { bars: number }) {
  return (
    <span className="inline-flex items-end gap-[2px] align-middle" aria-label={`${bars}/4 bars`}>
      {[1, 2, 3, 4].map((n) => (
        <span
          key={n}
          className={n <= bars ? "bg-emerald-400" : "bg-slate-700"}
          style={{ width: 3, height: 3 + n * 2, borderRadius: 1 }}
        />
      ))}
    </span>
  )
}

interface ProcessProgress {
  current: number
  total: number
}
interface ProgressSample {
  time: number
  current: number
}
const RATE_WINDOW = 6

function computeETA(
  current: number,
  total: number,
  history: ProgressSample[]
): string | null {
  if (history.length < 2) return null
  const oldest = history[0]
  const newest = history[history.length - 1]
  const elapsed = (newest.time - oldest.time) / 1000
  const done = newest.current - oldest.current
  if (done <= 0 || elapsed < 5) return null
  const rate = done / elapsed
  const remaining = (total - current) / rate
  if (!isFinite(remaining) || remaining <= 0) return null
  if (remaining < 60) return `~${Math.round(remaining)}s`
  if (remaining < 3600) return `~${Math.round(remaining / 60)}m`
  return `~${(remaining / 3600).toFixed(1)}h`
}

export default function Dashboard() {
  const [status, setStatus] = useState<PiStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [uptime, setUptime] = useState(0)
  const [driveStats, setDriveStats] = useState<DriveStats | null>(null)
  const [storageBreakdown, setStorageBreakdown] =
    useState<StorageBreakdown | null>(null)
  const [archiveProgress, setArchiveProgress] = useState<ProcessProgress | null>(null)
  const [processing, setProcessing] = useState(false)
  const [processProgress, setProcessProgress] = useState<ProcessProgress | null>(null)
  const [useFahrenheit, setUseFahrenheit] = useState(false)
  const [metric, setMetric] = useState(false)
  const [rtcWarning, setRtcWarning] = useState<string | null>(null)

  const archiveHistoryRef = useRef<ProgressSample[]>([])
  const processHistoryRef = useRef<ProgressSample[]>([])
  const updateInfo = useUpdateAvailable()
  const { status: awayStatus } = useAwayMode()
  const { mode: keepAwakeMode } = useKeepAwake()

  useEffect(() => {
    let mounted = true

    async function fetchStatus() {
      try {
        const data = await api.getStatus()
        if (!mounted) return
        setStatus(data)
        setUptime(parseFloat(data.uptime))
        setError(null)
      } catch {
        if (mounted) setError("Unable to connect to Sentry USB")
      }
    }

    async function fetchDriveStats() {
      try {
        const [stats, driveStatus] = await Promise.all([
          api.getDriveStats(),
          api.getDriveStatus(),
        ])
        if (!mounted) return
        setDriveStats(stats)
        setProcessing(driveStatus.running)
        if (!driveStatus.running) {
          setProcessProgress(null)
        } else if (driveStatus.process_total != null && driveStatus.process_total > 0) {
          setProcessProgress({
            current: driveStatus.process_current ?? 0,
            total: driveStatus.process_total,
          })
        }

        if (driveStatus.phase === "archiving" && driveStatus.total != null) {
          setArchiveProgress({
            current: driveStatus.current ?? 0,
            total: driveStatus.total,
          })
        } else {
          setArchiveProgress(null)
        }
      } catch {
        /* non-critical */
      }
    }

    async function fetchStorageBreakdown() {
      try {
        const data = await api.getStorageBreakdown()
        if (mounted) setStorageBreakdown(data)
      } catch {
        /* non-critical */
      }
    }

    fetchStatus()
    fetchDriveStats()
    fetchStorageBreakdown()

    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg.DRIVE_MAP_UNIT
        if (entry) {
          const val =
            typeof entry === "object" ? (entry.active ? entry.value : null) : entry
          if (val !== null) setMetric(val === "km")
        }
        const tempEntry = cfg.TEMPERATURE_UNIT
        if (tempEntry) {
          const val =
            typeof tempEntry === "object"
              ? (tempEntry.active ? tempEntry.value : null)
              : tempEntry
          if (val !== null) setUseFahrenheit(val === "F")
        }
      })
      .catch(() => {})

    fetch("/api/system/rtc-status")
      .then((r) => r.json())
      .then((rtc) => {
        if (mounted && rtc.is_pi5 && !rtc.rtc_healthy && rtc.battery_warning) {
          setRtcWarning(rtc.battery_warning)
        }
      })
      .catch(() => {})

    // Status drives the live-tile values (CPU, mem, temp). 2s is fast
    // enough that a glance still feels real-time and halves the
    // server hits vs the previous 1s cadence. The uptime tile uses a
    // separate local 1s interval below so the seconds counter still
    // advances smoothly between server polls.
    const statusInterval = setInterval(fetchStatus, 2000)
    const statsInterval = setInterval(fetchDriveStats, 5000)
    const storageInterval = setInterval(fetchStorageBreakdown, 10000)
    const uptimeInterval = setInterval(() => setUptime((p) => p + 1), 1000)

    const unsubscribe = wsClient.subscribe("drive_process", (data) => {
      if (!mounted) return
      const msg = data as { status: string; current?: number; total?: number }
      if (msg.status === "started") {
        setProcessing(true)
        setProcessProgress(null)
      } else if (
        msg.status === "progress" &&
        msg.current !== undefined &&
        msg.total !== undefined
      ) {
        setProcessing(true)
        setProcessProgress({ current: msg.current, total: msg.total })
      } else if (msg.status === "complete" || msg.status === "error") {
        setProcessing(false)
        setProcessProgress(null)
        fetchDriveStats()
      }
    })

    return () => {
      mounted = false
      clearInterval(statusInterval)
      clearInterval(statsInterval)
      clearInterval(storageInterval)
      clearInterval(uptimeInterval)
      unsubscribe()
    }
  }, [])

  useEffect(() => {
    if (archiveProgress && archiveProgress.current > 0) {
      const h = archiveHistoryRef.current
      h.push({ time: Date.now(), current: archiveProgress.current })
      if (h.length > RATE_WINDOW) h.shift()
    } else {
      archiveHistoryRef.current = []
    }
  }, [archiveProgress])

  useEffect(() => {
    if (processProgress && processProgress.current > 0) {
      const h = processHistoryRef.current
      h.push({ time: Date.now(), current: processProgress.current })
      if (h.length > RATE_WINDOW) h.shift()
    } else {
      processHistoryRef.current = []
    }
  }, [processProgress])

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center py-20">
        <Activity className="mb-4 h-12 w-12 text-slate-600" />
        <p className="text-lg font-medium text-slate-400">{error}</p>
        <p className="mt-1 text-sm text-slate-600">
          Make sure the Sentry USB API server is running
        </p>
      </div>
    )
  }

  if (!status) {
    return (
      <div className="space-y-4">
        <h1 className="text-2xl font-bold text-slate-100">Dashboard</h1>
        <div className="tile-grid">
          {[...Array(4)].map((_, i) => (
            <div key={i} className="glass-card h-32 animate-pulse" />
          ))}
        </div>
      </div>
    )
  }

  // Build banner stack — priority sorted (warn > update).
  const banners: BannerItem[] = []
  if (rtcWarning) {
    banners.push({
      id: "rtc",
      kind: "warn",
      icon: <AlertTriangle className="h-4 w-4" />,
      title: "RTC Battery Warning",
      sub: rtcWarning,
    })
  }
  if (updateInfo.available) {
    banners.push({
      id: "update",
      kind: "update",
      icon: <Download className="h-4 w-4" />,
      title: `Update Available${
        updateInfo.latestVersion ? `: ${updateInfo.latestVersion}` : ""
      }`,
      sub: "Go to Settings to install",
      action: (
        <Link
          to="/settings?tab=Updates"
          className="action-chip action-chip--accent shrink-0"
        >
          Install <ChevronRight className="h-3.5 w-3.5" />
        </Link>
      ),
    })
  }

  const isAwayActive = awayStatus.state === "active"

  return (
    <div className="space-y-3">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Dashboard</h1>
        <p className="mt-0.5 text-sm text-slate-500">System overview and status</p>
      </div>

      <BannerStack banners={banners} />

      <CloudStatusBar />

      <div className="tile-grid">
        <SystemTile
          status={status}
          uptime={uptime}
          useFahrenheit={useFahrenheit}
          keepAwakeIdle={keepAwakeMode == null}
        />
        <NetworkTile status={status} />
        <StorageTile
          status={status}
          breakdown={storageBreakdown}
        />
        <ActivityTile
          driveStats={driveStats}
          archiveProgress={archiveProgress}
          processProgress={processProgress}
          processing={processing}
          metric={metric}
          // eslint-disable-next-line react-hooks/refs -- ETA history is intentionally a ref (push-only, no re-render needed) and the original Dashboard read .current the same way.
          archiveEta={archiveProgress ? computeETA(archiveProgress.current, archiveProgress.total, archiveHistoryRef.current) : null}
          // eslint-disable-next-line react-hooks/refs -- same as above
          processEta={processProgress ? computeETA(processProgress.current, processProgress.total, processHistoryRef.current) : null}
        />
        <KeepAwakeTile />
        {isAwayActive && <AwayModeTile />}
      </div>

      {/* TirePressureCard renders below the tile grid because it needs
          the full row width for the multi-line history chart. The card
          self-fetches /api/telemetry/tire-history; it'll show an empty
          state when the BLE telemetry sampler hasn't run yet. */}
      <TirePressureCard />
    </div>
  )
}

// ─── Tiles ──────────────────────────────────────────────────────────────────

function SystemTile({
  status,
  uptime,
  useFahrenheit,
  keepAwakeIdle,
}: {
  status: PiStatus
  uptime: number
  useFahrenheit: boolean
  keepAwakeIdle: boolean
}) {
  const cpuTemp = parseInt(status.cpu_temp)
  return (
    <StatusTile
      icon={<Activity className="h-4 w-4" />}
      halo={getTempHalo(cpuTemp)}
      title="System"
    >
      <Row
        icon={<Clock className="h-3.5 w-3.5" />}
        label="Uptime"
        value={formatUptime(uptime)}
      />
      <Row
        icon={<Thermometer className="h-3.5 w-3.5" />}
        label="CPU"
        value={cpuTemp > 0 ? formatTemp(cpuTemp, useFahrenheit) : "N/A"}
        valueColor={cpuTemp > 0 ? getTempColor(cpuTemp) : undefined}
      />
      {status.fan_speed && (
        <Row
          icon={<Wind className="h-3.5 w-3.5" />}
          label="Fan"
          value={`${status.fan_speed} RPM`}
        />
      )}
      <Row
        icon={<HardDrive className="h-3.5 w-3.5" />}
        label="USB Drives"
        value={status.drives_active === "yes" ? "Connected" : "Disconnected"}
        valueColor={
          status.drives_active === "yes" ? "oklch(0.82 0.18 150)" : "#fbbf24"
        }
      />
      {keepAwakeIdle && (
        <Row
          icon={<HeartPulse className="h-3.5 w-3.5" />}
          label="Keep Awake"
          value={
            <Link
              to="/settings?tab=Device"
              className="text-blue-400 hover:text-blue-300"
            >
              Off
            </Link>
          }
        />
      )}
    </StatusTile>
  )
}

function NetworkTile({ status }: { status: PiStatus }) {
  const haveWifi = !!status.wifi_ssid
  const haveEth = !!status.ether_speed && status.ether_speed !== "Unknown!"
  const halo: Halo = haveWifi || haveEth ? "accent" : "red"

  return (
    <StatusTile
      icon={haveWifi || haveEth ? <Wifi className="h-4 w-4" /> : <WifiOff className="h-4 w-4" />}
      halo={halo}
      title="Network"
    >
      {haveWifi ? (
        <>
          <div className="tile-row">
            <span className="inline-flex text-slate-500">
              <Wifi className="h-3.5 w-3.5" />
            </span>
            <span className="text-xs font-medium text-slate-200">
              {status.wifi_ssid}
            </span>
            <span className="ml-auto inline-flex items-center gap-1.5 text-[10px] text-slate-500">
              {status.wifi_signal_dbm != null && (
                <span className="text-slate-400">{status.wifi_signal_dbm} dBm</span>
              )}
              <WifiBars bars={getWifiStrengthBars(status.wifi_strength)} />
            </span>
          </div>
          <div className="tile-row pl-5" style={{ minHeight: 18 }}>
            <span className="text-[10px] text-slate-500">{status.wifi_ip || "No IP"}</span>
            {(status.wifi_rx_bps !== undefined || status.wifi_tx_bps !== undefined) && (
              <>
                <span className="ml-auto text-[10px] text-emerald-400">
                  ↓ {formatThroughput(status.wifi_rx_bps ?? 0)}
                </span>
                <span className="text-[10px] text-slate-500">·</span>
                <span className="text-[10px] text-sky-400">
                  ↑ {formatThroughput(status.wifi_tx_bps ?? 0)}
                </span>
              </>
            )}
          </div>
        </>
      ) : (
        <Row
          icon={<WifiOff className="h-3.5 w-3.5" />}
          label="WiFi"
          sub="Not connected"
        />
      )}

      {haveEth ? (
        <>
          <div className="tile-row">
            <span className="inline-flex text-slate-500">
              <Cable className="h-3.5 w-3.5" />
            </span>
            <span className="text-xs font-medium text-slate-200">
              {status.ether_speed}
            </span>
            {status.ether_ip && (
              <span className="ml-auto text-[10px] text-slate-500">
                {status.ether_ip}
              </span>
            )}
          </div>
          {(status.ether_rx_bps !== undefined || status.ether_tx_bps !== undefined) && (
            <div className="tile-row pl-5" style={{ minHeight: 18 }}>
              <span className="text-[10px] text-emerald-400">
                ↓ {formatThroughput(status.ether_rx_bps ?? 0)}
              </span>
              <span className="text-[10px] text-slate-500">·</span>
              <span className="text-[10px] text-sky-400">
                ↑ {formatThroughput(status.ether_tx_bps ?? 0)}
              </span>
            </div>
          )}
        </>
      ) : (
        // Always render an Ethernet row — keeps tile balanced when WiFi is
        // present but ethernet isn't (or vice versa). Muted styling signals
        // disconnected state without taking the tile's halo over.
        <div className="tile-row">
          <span className="inline-flex text-slate-600">
            <Cable className="h-3.5 w-3.5" />
          </span>
          <span className="text-xs text-slate-600">Ethernet</span>
          <span className="ml-auto text-[10px] text-slate-600">Not connected</span>
        </div>
      )}
    </StatusTile>
  )
}

function StorageTile({
  status,
  breakdown,
}: {
  status: PiStatus
  breakdown: StorageBreakdown | null
}) {
  const totalSpace = parseInt(status.total_space)
  const freeSpace = parseInt(status.free_space)
  const usedSpace = totalSpace - freeSpace
  const usedPct = totalSpace > 0 ? (usedSpace / totalSpace) * 100 : 0
  const usedPctStr = totalSpace > 0 ? `${Math.round(usedPct)}%` : "0%"
  const snaps = parseInt(status.num_snapshots)

  const segments = breakdown
    ? [
        { label: "Dashcam", size: breakdown.cam_size, color: "#3b82f6" },
        { label: "Music", size: breakdown.music_size, color: "#a855f7" },
        { label: "Lightshow", size: breakdown.lightshow_size, color: "#f59e0b" },
        { label: "Boombox", size: breakdown.boombox_size, color: "#ec4899" },
        { label: "Snapshots", size: breakdown.snapshots_size, color: "#6366f1" },
      ].filter((s) => s.size > 0)
    : []

  return (
    <StatusTile
      icon={<HardDrive className="h-4 w-4" />}
      halo={getStorageHalo(usedPct)}
      title="Storage"
    >
      <div className="flex items-baseline gap-1.5">
        <span className="text-sm font-semibold text-slate-100">
          {formatBytes(usedSpace)}
        </span>
        <span className="text-[11px] text-slate-500">
          / {formatBytes(totalSpace)} · {usedPctStr} used
        </span>
      </div>
      {breakdown && segments.length > 0 ? (
        <>
          <div className="seg-bar">
            {segments.map((s) => (
              <div
                key={s.label}
                style={{
                  width: `${Math.max((s.size / breakdown.total_space) * 100, 0.5)}%`,
                  backgroundColor: s.color,
                }}
                title={`${s.label}: ${formatBytes(s.size)}`}
              />
            ))}
          </div>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
            {segments.map((s) => (
              <div key={s.label} className="flex items-center gap-1.5 text-[10px]">
                <span
                  className="inline-block h-1.5 w-1.5 rounded-full"
                  style={{ backgroundColor: s.color }}
                />
                <span className="text-slate-400">{s.label}</span>
                <span className="font-medium text-slate-300">
                  {formatBytes(s.size)}
                </span>
              </div>
            ))}
            <div className="flex items-center gap-1.5 text-[10px]">
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-slate-700" />
              <span className="text-slate-400">Free</span>
              <span className="font-medium text-slate-300">
                {formatBytes(breakdown.free_space)}
              </span>
            </div>
          </div>
        </>
      ) : (
        <div className="bar">
          <div
            className="bg-gradient-to-r from-blue-500 to-blue-400"
            style={{ width: `${usedPct}%` }}
          />
        </div>
      )}
      <TileDivider />
      <Row
        icon={<Camera className="h-3.5 w-3.5" />}
        label={`${snaps.toLocaleString()} snapshots`}
        sub={
          snaps > 0
            ? `${new Date(
                parseInt(status.snapshot_oldest) * 1000
              ).toLocaleDateString()} → ${new Date(
                parseInt(status.snapshot_newest) * 1000
              ).toLocaleDateString()}`
            : "—"
        }
      />
    </StatusTile>
  )
}

function ActivityTile({
  driveStats,
  archiveProgress,
  processProgress,
  processing,
  metric,
  archiveEta,
  processEta,
}: {
  driveStats: DriveStats | null
  archiveProgress: ProcessProgress | null
  processProgress: ProcessProgress | null
  processing: boolean
  metric: boolean
  archiveEta: string | null
  processEta: string | null
}) {
  const phase = archiveProgress
    ? ("archiving" as const)
    : processing
    ? ("processing" as const)
    : null
  const badge = phase ? (
    <Pill kind={phase === "archiving" ? "accent" : "sky"}>
      <LiveDot /> {phase}
    </Pill>
  ) : null

  return (
    <StatusTile
      icon={<Zap className="h-4 w-4" />}
      halo="violet"
      title="Activity"
      badge={badge}
    >
      {driveStats ? (
        driveStats.processed_count === 0 && driveStats.drives_count === 0 ? (
          <p className="t-xs">
            No drives processed yet. Plug a Sentry USB to ingest dashcam footage.
          </p>
        ) : (
          <>
            <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 text-xs">
              <span>
                <span className="text-sm font-semibold text-slate-100">
                  {driveStats.processed_count.toLocaleString()}
                </span>{" "}
                <span className="text-slate-500">clips</span>
              </span>
              <span>
                <span className="text-sm font-semibold text-slate-100">
                  {driveStats.drives_count.toLocaleString()}
                </span>{" "}
                <span className="text-slate-500">drives</span>
              </span>
              <span>
                <span className="text-sm font-semibold text-slate-100">
                  {metric
                    ? driveStats.total_distance_km.toFixed(0)
                    : driveStats.total_distance_mi.toFixed(0)}
                </span>{" "}
                <span className="text-slate-500">{metric ? "km" : "mi"}</span>
              </span>
              {driveStats.fsd_engaged_ms > 0 && (
                <Link
                  to="/fsd"
                  className="ml-auto flex items-center gap-1 text-[10px] text-emerald-400 transition-colors hover:text-emerald-300"
                >
                  <Zap className="h-3 w-3" />
                  FSD {driveStats.fsd_percent}%
                  <ChevronRight className="h-3 w-3 text-slate-600" />
                </Link>
              )}
            </div>

            {archiveProgress && archiveProgress.total > 0 ? (
              <ProgressBlock
                current={archiveProgress.current}
                total={archiveProgress.total}
                eta={archiveEta}
                color="emerald"
              />
            ) : processProgress && processProgress.total > 0 ? (
              <ProgressBlock
                current={processProgress.current}
                total={processProgress.total}
                eta={processEta}
                color="blue"
              />
            ) : processing ? (
              <div className="bar">
                <div
                  className="w-2/5 animate-pulse bg-gradient-to-r from-blue-500 to-blue-400"
                />
              </div>
            ) : null}
          </>
        )
      ) : (
        <>
          <div className="h-3 w-1/2 animate-pulse rounded bg-slate-800" />
          <div className="h-1.5 w-full animate-pulse rounded-full bg-slate-800" />
        </>
      )}
    </StatusTile>
  )
}

function ProgressBlock({
  current,
  total,
  eta,
  color,
}: {
  current: number
  total: number
  eta: string | null
  color: "emerald" | "blue"
}) {
  const pct = (current / total) * 100
  const grad =
    color === "emerald"
      ? "bg-gradient-to-r from-emerald-500 to-emerald-400"
      : "bg-gradient-to-r from-blue-500 to-blue-400"
  return (
    <>
      <div className="flex items-center justify-between text-[10px] text-slate-500 t-num">
        <span>
          {current.toLocaleString()} / {total.toLocaleString()}
          {eta && (
            <span
              className={`ml-1.5 ${
                color === "emerald" ? "text-emerald-400/70" : "text-blue-400/70"
              }`}
            >
              {eta}
            </span>
          )}
        </span>
        <span>{Math.round(pct)}%</span>
      </div>
      <div className="bar">
        <div className={grad} style={{ width: `${pct}%` }} />
      </div>
    </>
  )
}

const KEEP_AWAKE_DURATIONS = [
  { label: "15m", value: 15 },
  { label: "30m", value: 30 },
  { label: "1h", value: 60 },
  { label: "2h", value: 120 },
]

function KeepAwakeTile() {
  const { status, mode, start, stop } = useKeepAwake()
  const [showDurations, setShowDurations] = useState(false)

  if (!mode) return null

  const isActive = status.state === "active"
  const isPending = status.state === "pending"
  const isIdle = status.state === "idle"
  const remainingMin = status.remaining_sec ? Math.ceil(status.remaining_sec / 60) : 0

  const halo: Halo = isActive ? "rose" : isPending ? "amber" : "blue"
  const value = isActive
    ? `${remainingMin}m`
    : isPending
    ? "Pending"
    : mode === "auto"
    ? "Auto"
    : "Idle"
  const sub = isActive
    ? "Keeping car awake"
    : isPending
    ? "Waiting for archive..."
    : mode === "auto"
    ? "Activates on interaction"
    : "Tap to start"

  return (
    <StatusTile
      icon={
        isActive ? (
          <HeartPulse className="h-4 w-4 animate-pulse" />
        ) : isPending ? (
          <Timer className="h-4 w-4 animate-pulse" />
        ) : (
          <HeartPulse className="h-4 w-4" />
        )
      }
      halo={halo}
      title="Keep Awake"
      action={
        mode === "manual" && isIdle ? (
          <div className="relative">
            <button
              onClick={() => setShowDurations(!showDurations)}
              className="rounded-lg bg-blue-500/20 px-2.5 py-1 text-[11px] font-medium text-blue-400 transition-colors hover:bg-blue-500/30"
            >
              Start
            </button>
            {showDurations && (
              <div className="absolute right-0 top-full z-10 mt-1 w-28 rounded-lg border border-white/10 bg-slate-900 p-1 shadow-xl">
                {KEEP_AWAKE_DURATIONS.map((opt) => (
                  <button
                    key={opt.value}
                    onClick={() => {
                      start(opt.value)
                      setShowDurations(false)
                    }}
                    className="w-full rounded-md px-3 py-1.5 text-left text-xs text-slate-300 hover:bg-white/5"
                  >
                    {opt.label}
                  </button>
                ))}
              </div>
            )}
          </div>
        ) : isActive || isPending ? (
          <button
            onClick={stop}
            className="rounded-lg bg-red-500/15 px-2.5 py-1 text-[11px] font-medium text-red-400 transition-colors hover:bg-red-500/25"
          >
            Stop
          </button>
        ) : null
      }
    >
      <div className="flex items-baseline gap-2">
        <span className="text-lg font-semibold text-slate-100">{value}</span>
      </div>
      <p className="t-xs">{sub}</p>
    </StatusTile>
  )
}

function AwayModeTile() {
  const { status } = useAwayMode()
  const remaining = status.remaining_sec ?? 0
  const h = Math.floor(remaining / 3600)
  const m = Math.floor((remaining % 3600) / 60)

  let totalSec = 0
  if (status.enabled_at && status.expires_at) {
    totalSec =
      (new Date(status.expires_at).getTime() -
        new Date(status.enabled_at).getTime()) /
      1000
  }
  const pct = totalSec > 0 ? ((totalSec - remaining) / totalSec) * 100 : 0

  return (
    <StatusTile
      icon={<Wifi className="h-4 w-4" />}
      halo="blue"
      title="Away Mode"
      badge={
        <Pill kind="sky">
          <LiveDot /> Active
        </Pill>
      }
    >
      <div className="flex items-baseline gap-1.5">
        <span className="text-lg font-semibold text-slate-100">
          {h}h {m}m
        </span>
        <span className="t-xs">remaining</span>
      </div>
      <div className="bar">
        <div className="bg-sky-400" style={{ width: `${pct}%` }} />
      </div>
      {status.ap_ssid && (
        <p className="t-xs">
          AP <span className="t-mono text-slate-300">{status.ap_ssid}</span>
        </p>
      )}
    </StatusTile>
  )
}
