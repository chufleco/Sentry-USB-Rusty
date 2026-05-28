import { useMemo, useRef } from "react"
import {
  Area,
  AreaChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import { useScrubberActions } from "@/hooks/useScrubberSync"

interface DriveChartProps {
  series: { index: number; time: number; value: number }[]
  valueLabel: string
  valueFormatter: (n: number) => string
  startTime: string
}

// Chart layout constants — must stay in sync with the AreaChart's
// `margin` prop, YAxis `width`, and XAxis `padding` below. We compute
// the click→index mapping in pixel space ourselves because Recharts
// 3.x's `onClick` handler doesn't reliably populate
// `activeTooltipIndex` (the event fires before the chart's redux
// store settles).
//
// For a `type="number"` XAxis, data is drawn inside
//   [margin.left + yAxis.width + xPad.left,
//    containerWidth - margin.right - xPad.right]
// Skipping `margin.left` and the XAxis padding shifts the mapped time
// by ~(margin.left + xPad.left + xPad.right) / plotWidth of the drive
// duration — visible as ~30-60 sec of click-vs-map drift on a long
// drive.
const LEFT_MARGIN = 4
const RIGHT_MARGIN = 16
const YAXIS_WIDTH = 36
const X_PADDING_LEFT = 10
const X_PADDING_RIGHT = 4

export default function DriveChart({
  series,
  valueLabel,
  valueFormatter,
  startTime,
}: DriveChartProps) {
  const { setIndex } = useScrubberActions()
  const baseMs = useMemo(() => new Date(startTime).getTime(), [startTime])
  const containerRef = useRef<HTMLDivElement>(null)

  // Click anywhere in the chart → seek to that fractional position
  // along the time axis. The XAxis is `type="number"` keyed on `time`,
  // so Recharts positions points by their time value, NOT by array
  // index. Sample density is non-uniform (each 1-minute clip has a
  // different point count, plus small gaps can sit between clips), so
  // mapping click-X to an array index would land on a different sample
  // than the tooltip shows at the cursor. Resolve click-X → target
  // time, then binary-search the (time-sorted) series for the nearest
  // sample.
  const handleClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (series.length < 2) return
    const container = containerRef.current
    if (!container) return
    const rect = container.getBoundingClientRect()
    const plotLeft = LEFT_MARGIN + YAXIS_WIDTH + X_PADDING_LEFT
    const plotRight = rect.width - RIGHT_MARGIN - X_PADDING_RIGHT
    const plotWidth = plotRight - plotLeft
    if (plotWidth <= 0) return
    const x = e.clientX - rect.left
    const clamped = Math.max(plotLeft, Math.min(plotRight, x))
    const frac = (clamped - plotLeft) / plotWidth
    const tMin = series[0].time
    const tMax = series[series.length - 1].time
    if (!(tMax > tMin)) return
    const target = tMin + frac * (tMax - tMin)
    let lo = 0
    let hi = series.length - 1
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (series[mid].time < target) lo = mid + 1
      else hi = mid
    }
    let best = lo
    if (lo > 0) {
      const a = Math.abs(series[lo - 1].time - target)
      const b = Math.abs(series[lo].time - target)
      if (a < b) best = lo - 1
    }
    setIndex(series[best].index)
  }

  return (
    <div
      ref={containerRef}
      className="h-56 w-full cursor-crosshair select-none"
      onClick={handleClick}
      aria-label={`${valueLabel} chart`}
    >
      <ResponsiveContainer minHeight={0} minWidth={0}>
        <AreaChart
          data={series}
          margin={{ top: 10, right: RIGHT_MARGIN, bottom: 16, left: LEFT_MARGIN }}
        >
          <defs>
            <linearGradient id="speedFill" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="#34d399" stopOpacity={0.45} />
              <stop offset="100%" stopColor="#34d399" stopOpacity={0} />
            </linearGradient>
          </defs>
          <XAxis
            dataKey="time"
            type="number"
            domain={["dataMin", "dataMax"]}
            tickFormatter={(t: number) => formatTickTime(baseMs, t)}
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickLine={false}
            axisLine={false}
            tickMargin={10}
            minTickGap={56}
            padding={{ left: X_PADDING_LEFT, right: X_PADDING_RIGHT }}
          />
          <YAxis
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={(n: number) => Math.round(n).toString()}
            tickLine={false}
            axisLine={false}
            tickMargin={4}
            width={YAXIS_WIDTH}
          />
          <Tooltip
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null
              const p = payload[0].payload as {
                index: number
                time: number
                value: number
              }
              return (
                <div className="rounded-md border border-white/10 bg-slate-900/95 px-2 py-1 text-xs text-slate-200 shadow-xl">
                  <div className="font-medium tabular-nums">
                    {valueFormatter(p.value)}
                  </div>
                  <div className="text-[10px] text-slate-500 tabular-nums">
                    {formatTooltipTime(baseMs, p.time)}
                  </div>
                </div>
              )
            }}
            cursor={{ stroke: "#34d399", strokeWidth: 1, strokeOpacity: 0.6 }}
          />
          <Area
            type="monotone"
            dataKey="value"
            stroke="#34d399"
            strokeWidth={1.75}
            fill="url(#speedFill)"
            isAnimationActive={false}
          />
        </AreaChart>
      </ResponsiveContainer>
    </div>
  )
}

function formatTickTime(baseMs: number, relMs: number): string {
  const t = new Date(baseMs + relMs)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}

// Tooltip needs second-level resolution so the user can verify the
// clicked sample matches what the map's playback card shows — the
// map card already includes seconds. Axis ticks keep minute
// resolution (cleaner on a 30-min+ drive).
function formatTooltipTime(baseMs: number, relMs: number): string {
  const t = new Date(baseMs + relMs)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  })
}
