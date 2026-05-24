import { useMemo } from "react"
import {
  Area,
  AreaChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import { useScrubberSync } from "@/hooks/useScrubberSync"

interface DriveChartProps {
  series: { index: number; time: number; value: number }[]
  valueLabel: string
  valueFormatter: (n: number) => string
  startTime: string
}

export default function DriveChart({
  series,
  valueLabel,
  valueFormatter,
  startTime,
}: DriveChartProps) {
  const scrubber = useScrubberSync()
  const baseMs = useMemo(() => new Date(startTime).getTime(), [startTime])

  return (
    <div className="h-44 w-full" aria-label={`${valueLabel} chart`}>
      <ResponsiveContainer>
        <AreaChart
          data={series}
          margin={{ top: 8, right: 4, bottom: 0, left: 0 }}
          onMouseMove={(s) => {
            const idx = s?.activeTooltipIndex
            if (typeof idx === "number" && idx >= 0 && idx < series.length) {
              scrubber.setIndex(series[idx].index)
            }
          }}
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
            minTickGap={48}
          />
          <YAxis
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={valueFormatter}
            tickLine={false}
            axisLine={false}
            width={42}
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
                    {formatTickTime(baseMs, p.time)}
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
