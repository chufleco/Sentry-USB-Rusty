export function formatDuration(ms: number): string {
  const totalMin = Math.max(0, Math.floor(ms / 60000))
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  return `${h}h ${m}m`
}

export function formatHvacRuntime(seconds: number): string {
  const totalMin = Math.max(0, Math.floor(seconds / 60))
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  return `${h}h ${m}m`
}

export function formatDistance(mi: number, km: number, metric: boolean): string {
  const value = metric ? km : mi
  const unit = metric ? "km" : "mi"
  return `${value.toFixed(2)} ${unit}`
}

export function formatSpeed(mph: number, kmh: number, metric: boolean): string {
  const value = Math.round(metric ? kmh : mph)
  const unit = metric ? "km/h" : "mph"
  return `${value} ${unit}`
}

export function formatTempC(c: number | undefined, metric: boolean): string {
  if (c === undefined) return "—"
  if (metric) return `${Math.round(c)}°C`
  return `${Math.round((c * 9) / 5 + 32)}°F`
}

export function formatRelativeTime(iso: string, now: Date = new Date()): string {
  const t = new Date(iso)
  if (Number.isNaN(t.getTime())) return iso

  const sameDay = t.toDateString() === now.toDateString()
  const yesterday = new Date(now)
  yesterday.setDate(now.getDate() - 1)
  const isYesterday = t.toDateString() === yesterday.toDateString()

  const time = t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
  if (sameDay) return `Today ${time}`
  if (isYesterday) return `Yesterday ${time}`

  const diffMs = now.getTime() - t.getTime()
  const days = Math.floor(diffMs / (1000 * 60 * 60 * 24))
  if (days >= 0 && days < 7) {
    return `${t.toLocaleDateString([], { weekday: "long" })} ${time}`
  }
  return `${t.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`
}

export function formatPsi(psi: number | undefined): string {
  if (psi === undefined) return "—"
  return `${psi.toFixed(1)} psi`
}
