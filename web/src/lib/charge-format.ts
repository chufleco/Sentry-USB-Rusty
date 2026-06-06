// Display formatters for charge-session values. Each tolerates
// null/undefined and renders an em dash so the UI never shows "NaN" or
// "null" when a sample column was empty.

export function fmtEnergy(kwh: number | null | undefined): string {
  if (kwh == null) return "—"
  return `${kwh.toFixed(1)} kWh`
}

export function fmtSoc(pct: number | null | undefined): string {
  if (pct == null) return "—"
  return `${Math.round(pct)}%`
}

export function fmtPower(kw: number | null | undefined): string {
  if (kw == null) return "—"
  return `${Math.round(kw)} kW`
}

export function fmtRange(mi: number | null | undefined): string {
  if (mi == null) return "—"
  return `${Math.round(mi)} mi`
}

export function fmtDuration(secs: number | null | undefined): string {
  if (secs == null || secs <= 0) return "—"
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m`
  return `${secs}s`
}
