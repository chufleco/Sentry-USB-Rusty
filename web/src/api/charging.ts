import type {
  ChargeSessionDetail,
  ChargeSessionSummary,
  CurrentCharge,
} from "@/types/charging"

export async function fetchChargeSessions(): Promise<ChargeSessionSummary[]> {
  const res = await fetch("/api/charging")
  if (!res.ok) throw new Error(`charging: ${res.status}`)
  const data = await res.json()
  return Array.isArray(data.sessions) ? data.sessions : []
}

export async function fetchChargeSession(
  id: string | number,
): Promise<ChargeSessionDetail> {
  const res = await fetch(`/api/charging/${id}`)
  if (!res.ok) throw new Error(`charge session ${id}: ${res.status}`)
  return res.json()
}

/// Live "is the car charging right now" for the dashboard banner.
export async function fetchCurrentCharge(): Promise<CurrentCharge> {
  const res = await fetch("/api/charging/current")
  if (!res.ok) throw new Error(`charging/current: ${res.status}`)
  return res.json()
}

/// Every charge tag in use (for the filter + per-tag rate editor).
export async function fetchChargeTags(): Promise<string[]> {
  const res = await fetch("/api/charging/tags")
  if (!res.ok) throw new Error(`charging tags: ${res.status}`)
  const data = await res.json()
  return Array.isArray(data) ? data : []
}

/// Replace the tags for a charge session (id == its start timestamp).
export async function setChargeTags(
  id: string | number,
  tags: string[],
): Promise<void> {
  const res = await fetch(`/api/charging/${id}/tags`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tags }),
  })
  if (!res.ok) throw new Error(`set charge tags ${id}: ${res.status}`)
}
