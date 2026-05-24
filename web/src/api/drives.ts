import type { DriveDetail, DriveSummary, RouteOverview } from "@/types/drives"

export async function fetchDrives(): Promise<DriveSummary[]> {
  const res = await fetch("/api/drives")
  if (!res.ok) throw new Error(`drives: ${res.status}`)
  return res.json()
}

export async function fetchDriveDetail(id: string | number): Promise<DriveDetail> {
  const res = await fetch(`/api/drives/${id}`)
  if (!res.ok) throw new Error(`drive ${id}: ${res.status}`)
  return res.json()
}

export async function fetchRouteOverviews(maxPoints = 20): Promise<RouteOverview[]> {
  const res = await fetch(`/api/drives/routes?max_points=${maxPoints}`)
  if (!res.ok) throw new Error(`routes: ${res.status}`)
  return res.json()
}

export async function setDriveTags(id: string | number, tags: string[]): Promise<void> {
  const res = await fetch(`/api/drives/${id}/tags`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tags }),
  })
  if (!res.ok) throw new Error(`set tags ${id}: ${res.status}`)
}

export async function fetchTags(): Promise<string[]> {
  const res = await fetch("/api/drives/tags")
  if (!res.ok) throw new Error(`tags: ${res.status}`)
  return res.json()
}
