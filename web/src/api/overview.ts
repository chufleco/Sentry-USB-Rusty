// Page-load aggregate (experimental).
//
// `GET /api/overview` collapses the dashboard's cold-paint burst of small
// reads into a single round trip. The backend invokes the SAME handlers the
// singleton endpoints use and nests each one's body VERBATIM, so the parts
// below are exactly the shapes the per-tile endpoints already return — hence
// we reuse the existing TS types rather than redefining them.
//
// The endpoint is gated by SENTRYUSB_EXPERIMENTAL: when the flag is off the
// server returns 404 and this module is never used (the Dashboard only calls
// it when `useExperimental()` is true). It is purely additive — the legacy
// per-tile fetches remain the source of truth and the fallback on any error.

import type {
  PiStatus,
  StorageBreakdown,
  DriveStats,
  DriveStatus,
} from "@/lib/api"

/// A single sentryusb.conf entry as the setup-config handler emits it:
/// the raw value plus whether it's an active (exported) export vs a
/// commented-out default. Matches what Dashboard reads off `/api/setup/config`.
export interface OverviewConfigEntry {
  value: string
  active: boolean
}

/// Update-availability payload. The dedicated `useUpdateAvailable` hook reads
/// the same fields off `/api/update-status`; only `update_available` is load-
/// bearing for the dashboard, the rest are optional metadata.
export interface OverviewUpdateStatus {
  update_available?: boolean
  checked_at?: string
  [key: string]: unknown
}

/// Per-part failure record. When a sub-handler returns non-2xx the backend
/// nulls that part and records the status + the handler's own error body here,
/// keeping the overall envelope 200. A flaky tile never blanks the dashboard.
export interface OverviewError {
  status: number
  body: unknown
}

/// The aggregate envelope. Every part is the verbatim body of its singleton
/// endpoint (or `null` if that part failed — see `errors`). Reuses the
/// canonical types so this can never drift from the per-tile responses.
export interface Overview {
  status: PiStatus | null
  storageBreakdown: StorageBreakdown | null
  driveStats: DriveStats | null
  driveStatus: DriveStatus | null
  config: Record<string, OverviewConfigEntry> | null
  updateStatus: OverviewUpdateStatus | null
  /// Present always (possibly empty); keyed by the same names as the parts
  /// above for any sub-handler that failed.
  errors: Record<string, OverviewError>
}

/// Fetch the one-shot page-load aggregate. Throws on a non-2xx response
/// (notably 404 when the experimental flag is off) so callers can `.catch`
/// and fall back to the legacy per-tile fetches.
export async function getOverview(): Promise<Overview> {
  const res = await fetch("/api/overview", {
    headers: { Accept: "application/json" },
  })
  if (!res.ok) {
    throw new Error(`overview error: ${res.status} ${res.statusText}`)
  }
  return res.json() as Promise<Overview>
}
