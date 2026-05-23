import { useState, useEffect, useRef, useCallback } from "react"
import { Bluetooth, CheckCircle, AlertCircle, Loader2, Wifi, WifiOff, ChevronDown, ChevronUp, Eye, EyeOff } from "lucide-react"
import { cn } from "@/lib/utils"
import { wsClient } from "@/lib/ws"
import { PrefCard } from "@/components/settings/PrefCard"
import { Pill, LiveDot } from "@/components/ui/Pill"

type BleState =
  | "loading"
  | "disabled"
  | "needs_vin"
  | "needs_install"
  | "installing"
  | "idle"
  | "initiating"
  | "waiting"
  | "polling"
  | "paired"
  | "error"

interface BleStatusResp {
  status: "not_paired" | "keys_generated" | "paired"
  vin?: string
  binaries_installed?: boolean
  note?: string
}

interface BleEnabledResp {
  enabled: boolean
}

interface BleConnectedResp {
  last_success_ts: number
  seconds_ago: number | null
  sample_count_10min: number
  /** "keep_awake" while archiveloop's nudge holds the radio,
   *  "telemetry" while our own sampler is mid-call, null when the
   *  radio is free. Lets the UI explain a stale pill as "paused"
   *  rather than "disconnected". */
  radio_owner: string | null
  /** True when archiveloop reports phase=="archiving" — the most
   *  common reason `radio_owner === "keep_awake"`. */
  archiving: boolean
}

interface BleLatestSample {
  ts: number | null
  seconds_ago?: number
  battery_pct?: number | null
  interior_temp_c?: number | null
  exterior_temp_c?: number | null
  hvac_on?: boolean | null
  tire_fl_psi?: number | null
  tire_fr_psi?: number | null
  tire_rl_psi?: number | null
  tire_rr_psi?: number | null
  odometer_mi?: number | null
  software_version?: string | null
  source?: string
}

/**
 * BLE pair card with inline VIN entry, lazy binary install, and a
 * live "connected" indicator. Always rendered in the Device tab —
 * gating is now via the master `BleEnableToggle` card next door.
 *
 * Pairing flow on click:
 *   1. Validate VIN locally (17 alphanumeric chars).
 *   2. If VIN differs from saved → POST /api/system/ble-vin.
 *   3. If binaries missing → POST /api/system/ble-install, wait for
 *      `ble_install_status` WebSocket "done" event.
 *   4. POST /api/system/ble-pair, follow ble_status WebSocket events
 *      and polling fallback.
 */
export function BlePairButton() {
  const [bleState, setBleState] = useState<BleState>("loading")
  const [bleMsg, setBleMsg] = useState("")
  const [vin, setVin] = useState("")
  const [savedVin, setSavedVin] = useState("")
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [binariesInstalled, setBinariesInstalled] = useState<boolean>(false)
  const [lastSuccessTs, setLastSuccessTs] = useState<number>(0)
  const [sampleCount10min, setSampleCount10min] = useState<number>(0)
  const [radioOwner, setRadioOwner] = useState<string | null>(null)
  const [archiving, setArchiving] = useState<boolean>(false)
  const [nowTs, setNowTs] = useState<number>(Math.floor(Date.now() / 1000))
  const [outputOpen, setOutputOpen] = useState(false)
  const [latestSample, setLatestSample] = useState<BleLatestSample | null>(null)
  const [sampleLoading, setSampleLoading] = useState(false)
  const [sampleFetchedAt, setSampleFetchedAt] = useState<number>(0)
  const [vinRevealed, setVinRevealed] = useState(false)
  // Unit pref: mirrors Drives.tsx — DRIVE_MAP_UNIT=="km" → metric
  // (km + °C), else imperial (mi + °F). Default to imperial since
  // that's the wizard default and most North-American Pi owners.
  const [metric, setMetric] = useState<boolean>(false)

  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const connPollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const tickRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const samplePollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  // ---------------------------------------------------------------------------
  // Initial state load
  // ---------------------------------------------------------------------------
  const reloadStatus = useCallback(async () => {
    try {
      const [enabledRes, statusRes] = await Promise.all([
        fetch("/api/system/ble-enabled").then((r) => r.json() as Promise<BleEnabledResp>),
        fetch("/api/system/ble-status?quick=true").then((r) => r.json() as Promise<BleStatusResp>),
      ])
      const en = Boolean(enabledRes?.enabled)
      setEnabled(en)
      setBinariesInstalled(Boolean(statusRes?.binaries_installed))
      const fetchedVin = statusRes?.vin ?? ""
      setVin(fetchedVin)
      setSavedVin(fetchedVin)

      if (!en) {
        setBleState("disabled")
        setBleMsg("BLE is disabled. Enable it in the toggle above to pair.")
        return
      }
      if (statusRes?.status === "paired") {
        setBleState("paired")
        setBleMsg("Paired — click to re-pair.")
        return
      }
      if (!fetchedVin) {
        setBleState("needs_vin")
        setBleMsg("Enter your Tesla's VIN to begin pairing.")
        return
      }
      if (!statusRes?.binaries_installed) {
        setBleState("needs_install")
        setBleMsg("BLE support not installed. Click to install + pair.")
        return
      }
      setBleState("idle")
      setBleMsg("")
    } catch {
      setEnabled(false)
      setBleState("error")
      setBleMsg("Could not load BLE status.")
    }
  }, [])

  useEffect(() => {
    reloadStatus()
  }, [reloadStatus])

  // ---------------------------------------------------------------------------
  // ble_status (pairing) WebSocket subscription
  // ---------------------------------------------------------------------------
  useEffect(() => {
    const unsub = wsClient.subscribe("ble_status", (data: unknown) => {
      const d = data as { status: string; error?: string; output?: string }
      if (d.status === "pairing") {
        setBleState("initiating")
        setBleMsg("Sending pairing request to car...")
      } else if (d.status === "error") {
        setBleState("error")
        const errMsg = d.error || "Unknown error"
        if (errMsg.includes("maximum number of BLE")) {
          setBleMsg("Too many BLE devices active. Turn off Bluetooth on nearby phone keys and try again.")
        } else if (errMsg.includes("timed out")) {
          setBleMsg("BLE connection timed out. Make sure the Pi is near the car and try again.")
        } else {
          setBleMsg(errMsg)
        }
        cleanup()
      } else if (d.status === "waiting") {
        setBleState("waiting")
        setBleMsg("Tap your keycard on the center console to confirm pairing.")
        startPairingPoll()
      }
    })
    return () => {
      unsub()
      cleanup()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // ---------------------------------------------------------------------------
  // ble_install_status WebSocket subscription
  // ---------------------------------------------------------------------------
  useEffect(() => {
    const unsub = wsClient.subscribe("ble_install_status", (data: unknown) => {
      const d = data as { status: string; message?: string; error?: string }
      if (d.status === "installing") {
        setBleState("installing")
        setBleMsg("Installing BLE support...")
      } else if (d.status === "progress") {
        setBleState("installing")
        if (d.message) setBleMsg(d.message)
      } else if (d.status === "done") {
        // Install completed — refresh status, then continue to pair.
        setBinariesInstalled(true)
        runPairAfterInstall()
      } else if (d.status === "error") {
        setBleState("error")
        setBleMsg(d.error || "Install failed")
      }
    })
    return () => unsub()
  }, [])

  // ---------------------------------------------------------------------------
  // Live connection indicator: poll /api/system/ble-connected every 10s
  // and tick the "Xs ago" label every second. The backend's
  // last_success_ts merges the webui process's own probe successes with
  // the sampler daemon's MAX(ts) from telemetry_samples, so this
  // indicator reflects both pairing-flow activity and the autonomous
  // sampler — without it the pill would say "Disconnected" while the
  // sampler was happily writing rows in another process.
  // ---------------------------------------------------------------------------
  useEffect(() => {
    let cancelled = false
    async function fetchConn() {
      try {
        const res = await fetch("/api/system/ble-connected")
        const d = (await res.json()) as BleConnectedResp
        if (!cancelled) {
          setLastSuccessTs(d?.last_success_ts || 0)
          setSampleCount10min(d?.sample_count_10min ?? 0)
          setRadioOwner(d?.radio_owner ?? null)
          setArchiving(Boolean(d?.archiving))
        }
      } catch {
        /* ignore */
      }
    }
    fetchConn()
    connPollRef.current = setInterval(fetchConn, 10_000)
    tickRef.current = setInterval(() => setNowTs(Math.floor(Date.now() / 1000)), 1_000)
    return () => {
      cancelled = true
      if (connPollRef.current) clearInterval(connPollRef.current)
      if (tickRef.current) clearInterval(tickRef.current)
    }
  }, [])

  // ---------------------------------------------------------------------------
  // "Show output" panel — polls the latest sample every 5s while open
  // so the user can watch values change in real time as a verification
  // step before driving off.
  // ---------------------------------------------------------------------------
  const fetchLatestSample = useCallback(async () => {
    setSampleLoading(true)
    try {
      const res = await fetch("/api/system/ble-latest-sample")
      const d = (await res.json()) as BleLatestSample
      setLatestSample(d)
      setSampleFetchedAt(Math.floor(Date.now() / 1000))
    } catch {
      /* leave previous value */
    } finally {
      setSampleLoading(false)
    }
  }, [])

  // Unit preference — mirror Drives.tsx's DRIVE_MAP_UNIT lookup.
  useEffect(() => {
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg?.DRIVE_MAP_UNIT
        if (!entry) return
        const val = typeof entry === "object"
          ? (entry.active ? entry.value : null)
          : entry
        if (val !== null) setMetric(val === "km")
      })
      .catch(() => { /* default imperial */ })
  }, [])

  useEffect(() => {
    if (!outputOpen) {
      if (samplePollRef.current) {
        clearInterval(samplePollRef.current)
        samplePollRef.current = null
      }
      return
    }
    fetchLatestSample()
    samplePollRef.current = setInterval(fetchLatestSample, 5_000)
    return () => {
      if (samplePollRef.current) clearInterval(samplePollRef.current)
      samplePollRef.current = null
    }
  }, [outputOpen, fetchLatestSample])

  // ---------------------------------------------------------------------------
  // Helpers
  // ---------------------------------------------------------------------------
  function cleanup() {
    if (pollRef.current) {
      clearInterval(pollRef.current)
      pollRef.current = null
    }
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current)
      timeoutRef.current = null
    }
  }

  function startPairingPoll() {
    cleanup()
    let count = 0
    pollRef.current = setInterval(async () => {
      count++
      try {
        const res = await fetch("/api/system/ble-status")
        if (res.ok) {
          const data = (await res.json()) as BleStatusResp
          if (data.status === "paired") {
            setBleState("paired")
            setBleMsg("Successfully paired with car!")
            cleanup()
            return
          }
        }
      } catch {
        /* ignore */
      }
      if (count >= 12) {
        setBleState("error")
        setBleMsg(
          "Pairing timed out. Make sure you tapped your keycard on the center console, then try again.",
        )
        cleanup()
      }
    }, 5000)
    timeoutRef.current = setTimeout(() => {
      if (bleState !== "paired" && bleState !== "error") {
        setBleState("error")
        setBleMsg("Pairing timed out. Please try again.")
        cleanup()
      }
    }, 65_000)
  }

  function validVin(v: string): boolean {
    const trimmed = v.trim().toUpperCase()
    return trimmed.length === 17 && /^[A-Z0-9]+$/.test(trimmed)
  }

  // Triggered by the install-done WebSocket event. The pair flow can't
  // start synchronously from handlePair because install is async.
  async function runPairAfterInstall() {
    setBleState("initiating")
    setBleMsg("Install complete. Sending pairing request...")
    try {
      const res = await fetch("/api/system/ble-pair", { method: "POST" })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || "Failed to initiate BLE pairing")
      }
    } catch (err) {
      setBleState("error")
      setBleMsg(err instanceof Error ? err.message : "Failed to initiate pairing")
    }
  }

  async function handlePair() {
    if (!enabled) {
      setBleMsg("BLE is disabled. Enable it in the toggle above first.")
      return
    }
    const vinUpper = vin.trim().toUpperCase()
    if (!validVin(vinUpper)) {
      setBleState("error")
      setBleMsg("VIN must be exactly 17 alphanumeric characters.")
      return
    }

    // 1. Persist VIN if changed.
    if (vinUpper !== savedVin) {
      try {
        const res = await fetch("/api/system/ble-vin", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ vin: vinUpper }),
        })
        if (!res.ok) {
          const data = await res.json().catch(() => ({}))
          throw new Error(data.error || "Failed to save VIN")
        }
        setSavedVin(vinUpper)
        setVin(vinUpper)
      } catch (err) {
        setBleState("error")
        setBleMsg(err instanceof Error ? err.message : "Failed to save VIN")
        return
      }
    }

    // 2. Lazy install if needed. Pair handshake kicks off from the
    //    install-done WebSocket handler so we don't race the binary
    //    install.
    if (!binariesInstalled) {
      setBleState("installing")
      setBleMsg("Installing BLE support...")
      try {
        const res = await fetch("/api/system/ble-install", { method: "POST" })
        if (!res.ok) {
          const data = await res.json().catch(() => ({}))
          throw new Error(data.error || "Failed to start install")
        }
        const data = (await res.json()) as { already_installed: boolean }
        if (data.already_installed) {
          // Install endpoint reports it was already installed — just
          // proceed straight to pair without waiting for WebSocket.
          setBinariesInstalled(true)
          runPairAfterInstall()
        }
      } catch (err) {
        setBleState("error")
        setBleMsg(err instanceof Error ? err.message : "Failed to start install")
      }
      return
    }

    // 3. Already installed — start pair immediately.
    runPairAfterInstall()
  }

  function handleReset() {
    cleanup()
    reloadStatus()
  }

  // ---------------------------------------------------------------------------
  // Rendering helpers
  // ---------------------------------------------------------------------------
  const isActive =
    bleState === "initiating" ||
    bleState === "waiting" ||
    bleState === "polling" ||
    bleState === "installing"

  const halo: "accent" | "red" | "amber" | "blue" | "slate" =
    bleState === "disabled"
      ? "slate"
      : bleState === "paired"
        ? "accent"
        : bleState === "error"
          ? "red"
          : isActive
            ? "amber"
            : "blue"

  const icon =
    bleState === "loading" ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin" />
    ) : isActive ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin" />
    ) : bleState === "paired" ? (
      <CheckCircle className="h-3.5 w-3.5" />
    ) : bleState === "error" ? (
      <AlertCircle className="h-3.5 w-3.5" />
    ) : (
      <Bluetooth className="h-3.5 w-3.5" />
    )

  // ── Live connection indicator ──────────────────────────────────────────────
  // Shown for paired devices. Reads `last_success_ts` from the backend
  // and renders one of three states based on freshness.
  //   < 60s   → "Connected" (green, with live dot)
  //   < 600s  → "Last seen Ns ago" (sky)
  //   else    → "Disconnected" (slate)
  const secondsAgo = lastSuccessTs > 0 ? Math.max(0, nowTs - lastSuccessTs) : null
  const showLive = bleState === "paired"
  // "Paused" reason: the keep-awake nudge owns the radio (typically
  // because archiveloop is mid-archive). Show that as the pill state
  // when fresh data has stopped landing — avoids the user thinking
  // pairing broke when really the sampler is just waiting its turn.
  const radioBusyForOther = radioOwner === "keep_awake"
  const showPaused =
    showLive &&
    radioBusyForOther &&
    (secondsAgo === null || secondsAgo >= 60)
  const pauseLabel = archiving ? "Paused — archiving" : "Paused — keep-awake"

  const liveLabel = showPaused
    ? pauseLabel
    : secondsAgo === null
      ? "Idle"
      : secondsAgo < 60
        ? "Connected"
        : secondsAgo < 600
          ? `Last seen ${formatAgo(secondsAgo)} ago`
          : "Disconnected"
  const liveKind: "accent" | "sky" | "slate" | "amber" = showPaused
    ? "amber"
    : secondsAgo !== null && secondsAgo < 60
      ? "accent"
      : secondsAgo !== null && secondsAgo < 600
        ? "sky"
        : "slate"
  const liveIcon = showPaused ? (
    <Loader2 className="h-3 w-3 animate-spin" />
  ) : secondsAgo !== null && secondsAgo < 60 ? (
    <LiveDot />
  ) : secondsAgo !== null && secondsAgo < 600 ? (
    <Wifi className="h-3 w-3" />
  ) : (
    <WifiOff className="h-3 w-3" />
  )

  // ── Top-right badge: shows pair status + live connection ───────────────────
  const badge = (() => {
    if (bleState === "paired") {
      return (
        <span className="flex items-center gap-1.5">
          <Pill kind="accent">Paired</Pill>
          {showLive && (
            <Pill kind={liveKind} className="flex items-center gap-1">
              {liveIcon}
              {liveLabel}
            </Pill>
          )}
        </span>
      )
    }
    if (bleState === "disabled") return <Pill kind="slate">Disabled</Pill>
    if (bleState === "needs_install") return <Pill kind="amber">Install needed</Pill>
    if (bleState === "needs_vin") return <Pill kind="amber">VIN needed</Pill>
    return null
  })()

  // ── Button label + handler ─────────────────────────────────────────────────
  const buttonLabel = (() => {
    if (bleState === "disabled") return "Disabled"
    if (bleState === "loading") return "Loading..."
    if (bleState === "installing") return "Installing..."
    if (bleState === "initiating") return "Starting..."
    if (bleState === "waiting") return "Waiting for keycard..."
    if (bleState === "polling") return "Pairing..."
    if (bleState === "paired") return "Re-pair"
    if (bleState === "error") return "Retry"
    if (bleState === "needs_install") return "Install + Pair"
    return "Pair BLE"
  })()

  const buttonDisabled =
    isActive ||
    bleState === "loading" ||
    bleState === "disabled" ||
    !validVin(vin)

  const buttonHandler = bleState === "error" ? handleReset : handlePair

  return (
    <PrefCard icon={icon} halo={halo} title="BLE Pairing" badge={badge}>
      {/* VIN input — always visible so users can update it any time.
          Masked by default once a full 17-char VIN is set, since
          screenshots of the settings page (like the ones users share
          for troubleshooting) shouldn't leak the VIN. The eye toggle
          reveals it; partial values (mid-edit) always show in full so
          typing isn't surprising. */}
      <label className="flex flex-col gap-1">
        <span className="t-xs text-slate-500">Tesla VIN (17 chars)</span>
        <div className="relative">
          <input
            type="text"
            value={
              !vinRevealed && vin.length === 17
                ? `${vin.slice(0, 3)}${"•".repeat(11)}${vin.slice(-3)}`
                : vin
            }
            maxLength={17}
            autoCapitalize="characters"
            spellCheck={false}
            readOnly={!vinRevealed && vin.length === 17}
            disabled={bleState === "disabled" || isActive || bleState === "loading"}
            onChange={(e) => setVin(e.target.value.toUpperCase())}
            placeholder="5YJ3E1EA..."
            className={cn(
              "w-full rounded-lg border bg-white/5 px-3 py-1.5 pr-9 font-mono text-xs uppercase tracking-wider text-slate-100",
              "placeholder-slate-600 outline-none transition focus:ring-1",
              vin && !validVin(vin)
                ? "border-red-500/50 focus:border-red-500/50 focus:ring-red-500/25"
                : "border-white/10 focus:border-blue-500/50 focus:ring-blue-500/25",
              "disabled:opacity-50",
              !vinRevealed && vin.length === 17 && "cursor-default",
            )}
          />
          {vin.length === 17 && (
            <button
              type="button"
              onClick={() => setVinRevealed((v) => !v)}
              disabled={bleState === "disabled" || isActive || bleState === "loading"}
              title={vinRevealed ? "Hide VIN" : "Reveal VIN"}
              aria-label={vinRevealed ? "Hide VIN" : "Reveal VIN"}
              className="absolute right-1.5 top-1/2 -translate-y-1/2 rounded p-1 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300 disabled:opacity-50"
            >
              {vinRevealed ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
            </button>
          )}
        </div>
      </label>

      <p
        className={cn(
          "text-xs",
          bleState === "paired"
            ? "text-emerald-400"
            : bleState === "error"
              ? "text-red-400"
              : bleState === "waiting"
                ? "font-medium text-amber-400"
                : "text-slate-500",
        )}
      >
        {bleMsg || "Initiate Bluetooth Low Energy pairing with your car."}
      </p>

      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={buttonHandler}
          disabled={buttonDisabled}
          className={cn(
            "self-start rounded-lg px-3 py-1.5 text-xs font-medium transition-colors disabled:opacity-50",
            bleState === "paired"
              ? "bg-white/5 text-slate-300 hover:bg-white/10"
              : bleState === "error"
                ? "bg-red-500/15 text-red-400 hover:bg-red-500/25"
                : "bg-blue-500/15 text-blue-400 hover:bg-blue-500/25",
          )}
        >
          {buttonLabel}
        </button>
        {/* "Show output" is only useful when there's actually data
            to show — gate on a recent successful round-trip OR a
            non-zero sample count in the last 10 min. */}
        {bleState === "paired" &&
          ((secondsAgo !== null && secondsAgo < 600) || sampleCount10min > 0) && (
            <button
              onClick={() => setOutputOpen((v) => !v)}
              className="inline-flex items-center gap-1 self-start rounded-lg bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              {outputOpen ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
              {outputOpen ? "Hide output" : "Show output"}
            </button>
          )}
      </div>

      {outputOpen && (
        <TelemetryOutputPanel
          sample={latestSample}
          loading={sampleLoading}
          metric={metric}
          fetchedSecondsAgo={
            sampleFetchedAt > 0 ? Math.max(0, nowTs - sampleFetchedAt) : null
          }
          onRefresh={async () => {
            // Also refetch the connection pill so the user sees
            // immediate visible feedback (Last seen Xm ago updates)
            // even when the sample row itself hasn't changed.
            await fetchLatestSample()
            try {
              const res = await fetch("/api/system/ble-connected")
              const d = (await res.json()) as BleConnectedResp
              setLastSuccessTs(d?.last_success_ts || 0)
              setSampleCount10min(d?.sample_count_10min ?? 0)
              setRadioOwner(d?.radio_owner ?? null)
              setArchiving(Boolean(d?.archiving))
            } catch { /* ignore */ }
          }}
          radioOwner={radioOwner}
          archiving={archiving}
        />
      )}
    </PrefCard>
  )
}

/** Inline panel showing the most recent telemetry sample. Polls every
 *  5s while open via the parent component's interval. */
function TelemetryOutputPanel({
  sample,
  loading,
  metric,
  fetchedSecondsAgo,
  onRefresh,
  radioOwner,
  archiving,
}: {
  sample: BleLatestSample | null
  loading: boolean
  metric: boolean
  fetchedSecondsAgo: number | null
  onRefresh: () => void
  radioOwner: string | null
  archiving: boolean
}) {
  const hasSample = sample !== null && sample.ts !== null
  // Sample is considered "stale" once it's older than 60s — visually
  // de-emphasize so the user understands they're looking at past
  // values, not a real-time read like the Tesla app shows.
  const isStale = hasSample && (sample?.seconds_ago ?? 0) > 60
  return (
    <div className="rounded-lg border border-white/5 bg-black/20 p-3 text-xs">
      <div className="mb-2 flex items-center justify-between">
        <span className="font-semibold text-slate-300">Live telemetry from car</span>
        <div className="flex items-center gap-2">
          {fetchedSecondsAgo !== null && (
            <span className="text-[10px] text-slate-600">
              polled {fetchedSecondsAgo}s ago
            </span>
          )}
          <button
            onClick={onRefresh}
            disabled={loading}
            className="inline-flex items-center gap-1 rounded bg-white/5 px-2 py-0.5 text-[10px] text-slate-400 hover:bg-white/10 disabled:opacity-50"
          >
            {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : null}
            Refresh
          </button>
        </div>
      </div>

      {!hasSample && (
        <p className="text-slate-500">
          No samples in the database yet. If the car is awake and the Pi is in
          range, this should populate within ~15 s.
        </p>
      )}

      {hasSample && sample && (
        <div className="space-y-1.5">
          <Row label="Battery" value={fmtPct(sample.battery_pct)} />
          {sample.odometer_mi != null && (
            <Row label="Odometer" value={fmtOdo(sample.odometer_mi, metric)} />
          )}
          <Row label="Interior temp" value={fmtTemp(sample.interior_temp_c, metric)} />
          <Row label="Exterior temp" value={fmtTemp(sample.exterior_temp_c, metric)} />
          <Row label="HVAC" value={fmtBool(sample.hvac_on)} />
          {sample.software_version && (
            <Row label="Tesla OS" value={sample.software_version} />
          )}
          {/* Battery cell temperature intentionally omitted: Tesla
              doesn't expose it via state-query APIs (BLE or REST).
              Only battery_heater_on (a boolean) is available. */}
          {/* TPMS — show the whole block only when at least one tire
              reading is present. Cars without TPMS, or runs where the
              `state tire-pressure` call failed, naturally hide it. */}
          {(sample.tire_fl_psi != null ||
            sample.tire_fr_psi != null ||
            sample.tire_rl_psi != null ||
            sample.tire_rr_psi != null) && (
            <>
              <div className="mt-1 border-t border-white/5 pt-1.5 text-[10px] uppercase tracking-wider text-slate-600">
                Tire pressure (psi)
              </div>
              <Row label="Front left" value={fmtPsi(sample.tire_fl_psi)} />
              <Row label="Front right" value={fmtPsi(sample.tire_fr_psi)} />
              <Row label="Rear left" value={fmtPsi(sample.tire_rl_psi)} />
              <Row label="Rear right" value={fmtPsi(sample.tire_rr_psi)} />
            </>
          )}
          {isStale && radioOwner === "keep_awake" && (
            <p className="pt-1 text-[10px] text-amber-400/80">
              {archiving
                ? "Sampler paused while the Pi is archiving — keep-awake is holding the BLE radio. New samples will resume once archive completes."
                : "Sampler paused — the keep-awake nudge is holding the BLE radio. New samples will resume once it finishes."}
            </p>
          )}
          {isStale && radioOwner !== "keep_awake" && (
            <p className="pt-1 text-[10px] text-amber-400/70">
              These values are {sample.seconds_ago}s old. The Tesla app reads
              live data over LTE — small differences from what's shown here
              are expected when the car has been driving or charging.
            </p>
          )}
          {sample.source === "body_controller" && (
            <p className="pt-1 text-[10px] text-slate-600">
              <code>body_controller</code> samples are taken while the car is
              asleep — temperatures and HVAC stay blank because reading them
              would wake the car.
            </p>
          )}
        </div>
      )}
    </div>
  )
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between">
      <span className="text-slate-500">{label}</span>
      <span className="font-mono text-slate-300">{value}</span>
    </div>
  )
}

/** Tesla's app shows battery as a whole-number percentage; matching
 *  that avoids the "Pi says 70%, app says 69%" confusion which is
 *  really just `.toFixed(1)` faking precision on a coarse integer
 *  value. */
function fmtPct(v: number | null | undefined): string {
  return v == null ? "—" : `${Math.round(v)}%`
}

/** Backend always stores Celsius (single source of truth). Convert
 *  for display when the user prefers Fahrenheit. */
function fmtTemp(v: number | null | undefined, metric: boolean): string {
  if (v == null) return "—"
  return metric ? `${v.toFixed(1)}°C` : `${(v * 9 / 5 + 32).toFixed(1)}°F`
}

function fmtBool(v: boolean | null | undefined): string {
  if (v == null) return "—"
  return v ? "on" : "off"
}

/** Tesla reports TPMS in PSI. We follow that — most users (US + EU)
 *  read tire pressure in PSI even when other units are metric. */
function fmtPsi(v: number | null | undefined): string {
  return v == null ? "—" : `${Math.round(v)} psi`
}

/** Odometer in miles (Tesla's native unit). When the user prefers
 *  metric, convert at display time so the source-of-truth in the DB
 *  stays single-unit. */
function fmtOdo(v: number | null | undefined, metric: boolean): string {
  if (v == null) return "—"
  return metric
    ? `${(v * 1.609344).toFixed(1)} km`
    : `${v.toFixed(1)} mi`
}

/** Compact relative-time formatter for the live indicator. */
function formatAgo(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`
  return `${Math.floor(seconds / 3600)}h`
}
