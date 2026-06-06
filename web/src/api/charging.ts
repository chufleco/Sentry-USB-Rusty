import type {
  ChargeSessionDetail,
  ChargeSessionSummary,
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
