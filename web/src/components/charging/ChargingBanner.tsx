import { useEffect, useState } from "react"
import { BatteryCharging } from "lucide-react"
import { fetchCurrentCharge } from "@/api/charging"
import type { CurrentCharge } from "@/types/charging"

const POLL_MS = 30_000

/** "3h 45m to full" / "45m to full" — null/0 renders nothing. */
function fmtToFull(mins: number | null): string | null {
  if (mins == null || mins <= 0) return null
  const h = Math.floor(mins / 60)
  const m = mins % 60
  if (h > 0) return `~${h}h ${m}m to full`
  return `~${m}m to full`
}

/**
 * Slim dashboard banner shown only while the car is actively charging.
 * Polls /api/charging/current; renders nothing when idle, so it occupies
 * zero space the rest of the time.
 */
export default function ChargingBanner() {
  const [cur, setCur] = useState<CurrentCharge | null>(null)

  useEffect(() => {
    let alive = true
    const tick = () =>
      fetchCurrentCharge()
        .then((c) => alive && setCur(c))
        .catch(() => alive && setCur(null))
    tick()
    const id = setInterval(tick, POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [])

  if (!cur?.charging) return null

  const toFull = fmtToFull(cur.minutesToFull)
  const parts: string[] = []
  if (cur.soc != null) {
    parts.push(
      cur.limitSoc != null
        ? `${Math.round(cur.soc)}% → ${cur.limitSoc}%`
        : `${Math.round(cur.soc)}%`,
    )
  }
  if (cur.powerKw != null) parts.push(`${cur.powerKw} kW`)
  if (toFull) parts.push(toFull)

  return (
    <div className="mb-4 flex items-center gap-2 rounded-xl border border-emerald-500/20 bg-emerald-500/10 px-4 py-2.5 text-sm text-emerald-300">
      <BatteryCharging className="h-4 w-4 shrink-0 animate-pulse" />
      <span className="font-medium text-emerald-200">Charging</span>
      {parts.length > 0 && (
        <span className="text-emerald-400/70">·</span>
      )}
      <span className="flex flex-wrap items-center gap-x-2 gap-y-0.5 tabular-nums">
        {parts.map((p, i) => (
          <span key={i} className="flex items-center gap-2">
            {i > 0 && <span className="text-emerald-400/50">·</span>}
            {p}
          </span>
        ))}
      </span>
    </div>
  )
}
