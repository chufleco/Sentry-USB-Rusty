import { useCallback, useEffect, useRef, useState } from "react"
import {
  CheckCircleIcon,
  ErrorIcon,
  MemoryIcon,
  ProgressActivityIcon,
  WarningIcon,
} from "@/components/icons"
import { Modal } from "@/components/ui/Modal"
import { wsClient } from "@/lib/ws"
import { cn } from "@/lib/utils"

export interface C6FirmwareStatus {
  present: boolean
  running_version: string | null
  candidate_version: string | null
  rollback_version: string | null
  update_available: boolean
  can_rollback: boolean
  can_recover: boolean
  install: InstallState
}

interface InstallState {
  state: "idle" | "running" | "success" | "failed" | "rolled_back"
  step: string
  progress: number
  message: string
  updated_at: number
  /** "install" | "rollback" | "" — which operation this progress belongs to. */
  op?: string
}

const IDLE: InstallState = {
  state: "idle",
  step: "",
  progress: 0,
  message: "",
  updated_at: 0,
  op: "",
}

/**
 * Installs a staged ESP32-C6 firmware update, or rolls back to the previous
 * one, with live progress. `intent` picks which the modal opens into.
 *
 * The flash reboots the ESP32-C6, so the car's BLE link drops for roughly
 * 20-30 seconds partway through — but the Pi's web server stays up the whole
 * time (only the C6 serial link pauses), so this page keeps its connection and
 * polls progress straight through. The flasher writes the app partition only,
 * so the chip's Tesla key is preserved; if the new image does not verify, the
 * previous one is restored automatically.
 */
export function C6FirmwareModal({
  status,
  intent,
  onClose,
  onRefresh,
}: {
  status: C6FirmwareStatus
  intent: "install" | "rollback" | "recover"
  onClose: () => void
  onRefresh: () => void
}) {
  // Start fresh on open: only inherit the server state if a flash is actually
  // running right now. A terminal state (success/failed/rolled_back) left over
  // from a previous operation must NOT show here, or reopening the box (e.g. via
  // the green "Up to date" chip) would display the last run instead of the
  // current install/rollback pitch.
  const [install, setInstall] = useState<InstallState>(
    status.install?.state === "running" ? status.install : IDLE
  )
  // The active operation. Starts at the intent the box was opened with, but the
  // in-box "Roll back to the previous firmware" link can switch an update box to
  // a rollback, so the title/footer must follow the actual operation, not the
  // original intent.
  const [mode, setMode] = useState<"install" | "rollback" | "recover">(intent)
  const [starting, setStarting] = useState(false)
  const [startError, setStartError] = useState("")
  const pollRef = useRef<number | null>(null)

  // `install` is the source of truth (seeded from the server only when it was
  // already running — see useState above). Do NOT also key off the parent's
  // `status.install` prop: it goes stale and would keep the modal locked
  // (non-dismissable, Try-again disabled) after the operation has finished.
  const running = install.state === "running" || starting
  const finished =
    install.state === "success" ||
    install.state === "failed" ||
    install.state === "rolled_back"

  // Live updates over the WebSocket…
  useEffect(() => {
    return wsClient.subscribe("c6_firmware_status", (msg: unknown) => {
      const m = msg as InstallState
      // Accept an idle snapshot regardless of timestamp — an orphaned "running"
      // is cleared to idle and must unstick the modal even if its ts is older.
      setInstall((prev) => (m.state === "idle" || m.updated_at >= prev.updated_at ? m : prev))
    })
  }, [])

  // …and a poll as a backstop while a flash is in flight.
  useEffect(() => {
    if (!running) {
      if (pollRef.current) {
        window.clearInterval(pollRef.current)
        pollRef.current = null
      }
      return
    }
    pollRef.current = window.setInterval(() => {
      fetch("/api/system/c6-firmware")
        .then((r) => r.json())
        .then((d: C6FirmwareStatus) => {
          // Only accept a newer snapshot — a poll that left the server while the
          // state was still "running" must not overwrite a fresher WS "success".
          if (d.install) {
            setInstall((prev) =>
              d.install.state === "idle" || d.install.updated_at >= prev.updated_at
                ? d.install
                : prev
            )
          }
        })
        .catch(() => {
          /* transient — keep polling */
        })
    }, 2000)
    return () => {
      if (pollRef.current) window.clearInterval(pollRef.current)
      pollRef.current = null
    }
  }, [running])

  // Refresh the parent (the toggle row) once it settles.
  useEffect(() => {
    if (finished) onRefresh()
  }, [finished, onRefresh])

  const start = useCallback(() => {
    setMode("install")
    setStarting(true)
    setStartError("")
    fetch("/api/system/c6-firmware/install", { method: "POST" })
      .then(async (r) => {
        if (!r.ok) {
          const body = await r.json().catch(() => ({}))
          throw new Error(body.error || "Could not start the update.")
        }
        setInstall({ ...IDLE, state: "running", message: "Starting…" })
      })
      .catch((e: Error) => setStartError(e.message))
      .finally(() => setStarting(false))
  }, [])

  const rollback = useCallback(() => {
    setMode("rollback")
    setStarting(true)
    setStartError("")
    fetch("/api/system/c6-firmware/rollback", { method: "POST" })
      .then(async (r) => {
        if (!r.ok) {
          const body = await r.json().catch(() => ({}))
          throw new Error(body.error || "Could not start the rollback.")
        }
        setInstall({ ...IDLE, state: "running", message: "Restoring…" })
      })
      .catch((e: Error) => setStartError(e.message))
      .finally(() => setStarting(false))
  }, [])

  const recoverChip = useCallback(() => {
    setMode("recover")
    setStarting(true)
    setStartError("")
    fetch("/api/system/c6-firmware/recover", { method: "POST" })
      .then(async (r) => {
        if (!r.ok) {
          const body = await r.json().catch(() => ({}))
          throw new Error(body.error || "Could not start setup.")
        }
        setInstall({ ...IDLE, state: "running", message: "Starting…" })
      })
      .catch((e: Error) => setStartError(e.message))
      .finally(() => setStarting(false))
  }, [])

  const isRollback = mode === "rollback"
  const isRecover = mode === "recover"
  const nothingToDo = isRollback && !status.can_rollback
  const primaryAction = isRecover ? recoverChip : isRollback ? rollback : start
  const target = isRecover
    ? "the ESP32-C6 firmware"
    : isRollback
      ? status.rollback_version ?? "the previous build"
      : status.candidate_version ?? "the firmware"
  const primaryLabel = isRecover
    ? "Set up ESP32-C6"
    : isRollback
      ? "Roll back"
      : "Update firmware"

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <MemoryIcon className="h-4 w-4 text-amber-400" />
          <span>
            {isRecover
              ? "Set up ESP32-C6"
              : isRollback
                ? "ESP32-C6 firmware rollback"
                : "ESP32-C6 firmware update"}
          </span>
        </span>
      }
      onClose={running ? () => {} : onClose}
      dismissable={!running}
      size="md"
      footer={
        <div className="flex items-center justify-between gap-3">
          <span className="text-[11px] text-slate-500">
            {install.state === "success"
              ? status.running_version
                ? `Running ${status.running_version}`
                : "Done"
              : install.state === "idle" && nothingToDo
                ? status.running_version
                  ? `Running ${status.running_version}`
                  : "Up to date"
                : status.running_version
                  ? `Running ${status.running_version} → ${target}`
                  : `Target ${target}`}
          </span>
          <div className="flex gap-2">
            {!running && install.state !== "success" && !nothingToDo && (
              <button
                onClick={onClose}
                className="rounded-lg bg-slate-500/15 px-3 py-1.5 text-xs font-medium text-slate-300 hover:bg-slate-500/25"
              >
                Not now
              </button>
            )}
            {install.state === "success" && !starting ? (
              <button
                onClick={onClose}
                className="rounded-lg bg-emerald-500/15 px-3 py-1.5 text-xs font-medium text-emerald-400 hover:bg-emerald-500/25"
              >
                Done
              </button>
            ) : nothingToDo && !running ? (
              <button
                onClick={onClose}
                className="rounded-lg bg-emerald-500/15 px-3 py-1.5 text-xs font-medium text-emerald-400 hover:bg-emerald-500/25"
              >
                Done
              </button>
            ) : install.state === "rolled_back" || install.state === "failed" ? (
              <button
                onClick={primaryAction}
                disabled={running}
                className="rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 hover:bg-blue-500/25 disabled:opacity-50"
              >
                Try again
              </button>
            ) : (
              <button
                onClick={primaryAction}
                disabled={running}
                className="rounded-lg bg-amber-500/15 px-3 py-1.5 text-xs font-medium text-amber-400 hover:bg-amber-500/25 disabled:opacity-50"
              >
                {running ? "Working…" : primaryLabel}
              </button>
            )}
          </div>
        </div>
      }
    >
      <div className="flex flex-col gap-3 text-xs leading-relaxed text-slate-400">
        {install.state === "idle" && !isRollback && !isRecover && (
          <p>
            The ESP32-C6 is running
            {" "}
            <span className="text-slate-300">{status.running_version ?? "unknown"}</span>.
            A newer build
            {" "}
            <span className="text-slate-300">{status.candidate_version ?? "is available"}</span>
            {" "}
            is ready to install.
          </p>
        )}

        {install.state === "idle" && isRecover && (
          <>
            <p>
              The ESP32-C6 is plugged in but not responding. This writes the
              firmware fresh to recover it.
            </p>
            <p className="text-[11px] text-amber-400/80">
              It reinstalls firmware and keeps the paired Tesla key when it can,
              re-provisioning it only if a full recovery is needed. Only do this if
              unplug/replug and the RESET button did not bring it back.
            </p>
          </>
        )}

        {install.state === "idle" && isRollback && !nothingToDo && (
          <p>
            The ESP32-C6 is running
            {" "}
            <span className="text-slate-300">{status.running_version ?? "unknown"}</span>.
            Roll back to the previously installed
            {" "}
            <span className="text-slate-300">{status.rollback_version ?? "build"}</span>.
          </p>
        )}

        {install.state === "idle" && nothingToDo && (
          <p>
            The ESP32-C6 is running
            {" "}
            <span className="text-slate-300">{status.running_version ?? "unknown"}</span>.
            There's no earlier version saved to roll back to.
          </p>
        )}

        {install.state === "idle" && !nothingToDo && !isRecover && (
          <p className="text-[11px] text-slate-500">
            The ESP32-C6 reboots during the flash, so the car's Bluetooth link drops for about
            20-30 seconds — this page stays connected and shows progress the whole time. Only the
            firmware's app partition is written, so the chip's paired Tesla key is kept. If the new
            image does not verify, SentryUSB tries to put the previous firmware back and tells you
            the result. Dashcam recording is not affected.
          </p>
        )}

        {(running || finished) && (
          <div className="flex flex-col gap-2.5 py-2">
            <div className="flex items-center gap-2">
              {install.state === "success" ? (
                <CheckCircleIcon className="h-4 w-4 shrink-0 text-emerald-400" />
              ) : install.state === "failed" ? (
                <ErrorIcon className="h-4 w-4 shrink-0 text-red-400" />
              ) : install.state === "rolled_back" ? (
                <WarningIcon className="h-4 w-4 shrink-0 text-amber-400" />
              ) : (
                <ProgressActivityIcon className="h-4 w-4 shrink-0 animate-spin text-blue-400" />
              )}
              <span
                className={cn(
                  "text-xs",
                  install.state === "success" && "text-emerald-300",
                  install.state === "failed" && "text-red-300",
                  install.state === "rolled_back" && "text-amber-300",
                  install.state === "running" && "text-slate-300"
                )}
              >
                {install.message || "Working…"}
              </span>
            </div>

            <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-500/20">
              <div
                className={cn(
                  "h-full rounded-full transition-all duration-500",
                  install.state === "success" && "bg-emerald-400",
                  install.state === "failed" && "bg-red-400",
                  install.state === "rolled_back" && "bg-amber-400",
                  install.state === "running" && "bg-blue-400"
                )}
                style={{ width: `${Math.max(install.progress, 4)}%` }}
              />
            </div>

            {running && (
              <p className="text-[11px] text-slate-500">
                Do not unplug the ESP32-C6. The car's Bluetooth link will come back on its own.
              </p>
            )}

            {/* After a successful install, offer rollback right here too. */}
            {install.state === "success" && !isRollback && status.can_rollback && (
              <button
                onClick={rollback}
                className="self-start text-[11px] text-slate-500 underline hover:text-slate-300"
              >
                Roll back to the previous firmware
              </button>
            )}
          </div>
        )}

        {startError && <p className="text-xs text-red-400">{startError}</p>}
      </div>
    </Modal>
  )
}
