import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import type { ChargePoint } from "@/types/charging"

export interface ChargeSeries {
  // Key into ChargePoint. Kept loose so callers can also pass derived
  // keys after mapping points; the chart only reads number | null.
  key: keyof ChargePoint
  name: string
  color: string
}

// One generic line chart over a charge session's per-sample points,
// plotting one or more series on a shared left axis. Mirrors the dark
// theme and tick styling of the drive charts so the charging detail
// reads as part of the same UI. Used for range, amperage, voltage and
// the temperature series — each is a separate card with its own unit.
export default function ChargingLineChart({
  points,
  series,
  unit,
  yDomain = [0, "auto"],
}: {
  points: ChargePoint[]
  series: ChargeSeries[]
  unit: string
  yDomain?: [number | string, number | string]
}) {
  return (
    <div className="h-48 w-full" aria-label={`${series.map((s) => s.name).join(", ")} chart`}>
      <ResponsiveContainer minHeight={0} minWidth={0}>
        <LineChart data={points} margin={{ top: 10, right: 8, bottom: 24, left: 4 }}>
          <CartesianGrid stroke="#1e242f" strokeDasharray="3 3" vertical={false} />
          <XAxis
            dataKey="ts"
            type="number"
            domain={["dataMin", "dataMax"]}
            tickFormatter={formatTick}
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickLine={false}
            axisLine={false}
            tickMargin={10}
            minTickGap={56}
          />
          <YAxis
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={(n: number) => `${Math.round(n)}${unit}`}
            tickLine={false}
            axisLine={false}
            tickMargin={4}
            width={48}
            domain={yDomain}
          />
          <Tooltip
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null
              const p = payload[0].payload as ChargePoint
              return (
                <div className="rounded-md border border-white/10 bg-slate-900/95 px-2 py-1.5 text-xs text-slate-200 shadow-xl">
                  <div className="mb-1 text-[10px] text-slate-500 tabular-nums">
                    {formatTooltipTime(p.ts)}
                  </div>
                  {series.map((s) => {
                    const v = p[s.key]
                    if (v == null || typeof v !== "number") return null
                    return (
                      <div key={String(s.key)} className="flex items-center gap-2 tabular-nums">
                        <span
                          className="inline-block h-2 w-2 rounded-full"
                          style={{ background: s.color }}
                          aria-hidden
                        />
                        <span className="text-slate-400">{s.name}</span>
                        <span className="ml-auto font-medium">
                          {Math.round(v)}
                          {unit}
                        </span>
                      </div>
                    )
                  })}
                </div>
              )
            }}
          />
          {series.length > 1 && (
            <Legend
              verticalAlign="bottom"
              height={20}
              iconType="line"
              wrapperStyle={{ fontSize: 11, color: "#94a3b8" }}
            />
          )}
          {series.map((s) => (
            <Line
              key={String(s.key)}
              type="monotone"
              name={s.name}
              dataKey={s.key as string}
              stroke={s.color}
              strokeWidth={2}
              dot={false}
              isAnimationActive={false}
              connectNulls
            />
          ))}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

function formatTick(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}

function formatTooltipTime(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  })
}
