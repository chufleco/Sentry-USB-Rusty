import { useEffect, useState, lazy, Suspense } from "react"
import { BrowserRouter, Routes, Route } from "react-router-dom"
import { Loader2 } from "lucide-react"
import { AppShell } from "@/components/layout/AppShell"
import { SetupWizard } from "@/components/setup/SetupWizard"
import { SetupProgress } from "@/components/setup/SetupProgress"
import { AuthProvider, useAuth } from "@/hooks/useAuth"

// Lazy routes — each page becomes its own JS chunk. Visiting the
// Dashboard no longer pulls in xterm (Terminal), leaflet (Viewer), or
// recharts (FSDAnalytics) on first paint. The Login screen is also
// lazy so unauthenticated users don't pay for the shell.
const Dashboard = lazy(() => import("@/pages/Dashboard"))
const Viewer = lazy(() => import("@/pages/Viewer"))
const Files = lazy(() => import("@/pages/Files"))
const Logs = lazy(() => import("@/pages/Logs"))
const Settings = lazy(() => import("@/pages/Settings"))
const Drives = lazy(() => import("@/pages/Drives"))
const DriveDetail = lazy(() => import("@/pages/DriveDetail"))
const Charging = lazy(() => import("@/pages/Charging"))
const ChargeSessionDetail = lazy(() => import("@/pages/ChargeSessionDetail"))
// Dev-only mock-data preview of in-progress UI; not routed in production.
const PreviewCharging = lazy(() => import("@/pages/PreviewCharging"))
const Support = lazy(() => import("@/pages/Support"))
const Terminal = lazy(() => import("@/pages/Terminal"))
const FSDAnalytics = lazy(() => import("@/pages/FSDAnalytics"))
const Community = lazy(() => import("@/pages/Community"))
const Notifications = lazy(() => import("@/pages/Notifications"))
const Snapshots = lazy(() => import("@/pages/Snapshots"))
const Login = lazy(() => import("@/pages/Login"))

type AppState = "loading" | "setup" | "configuring" | "finalizing" | "ready"

export default function App() {
  return (
    <AuthProvider>
      <AppContent />
    </AuthProvider>
  )
}

function AppContent() {
  const [appState, setAppState] = useState<AppState>("loading")
  const { state: authState, login } = useAuth()

  useEffect(() => {
    let cancelled = false
    async function checkStatus() {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (cancelled) return
        if (data.setup_finished) {
          setAppState("ready")
        } else if (data.setup_running) {
          setAppState("configuring")
        } else {
          setAppState("setup")
        }
      } catch {
        if (!cancelled) setAppState("ready")
      }
    }
    checkStatus()
    return () => { cancelled = true }
  }, [])

  // Poll while configuring — wait for setup to finish. The backend sets
  // SENTRYUSB_SETUP_FINISHED *before* the 5-second delay and final
  // `systemctl reboot`, so going straight to "ready" here would land the
  // user on the dashboard just in time for the Pi to kill the network.
  // Transition to "finalizing" first and let that effect wait for the
  // reboot to actually happen.
  useEffect(() => {
    if (appState !== "configuring") return
    const interval = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (data.setup_finished) {
          setAppState("finalizing")
        }
      } catch {
        // Server rebooting — keep polling
      }
    }, 3000)
    return () => clearInterval(interval)
  }, [appState])

  // Finalizing: wait for the server to drop off (confirming the final
  // reboot actually started) and then come back before redirecting to
  // the dashboard. Without the `wentDown` gate we could bounce straight
  // to "ready" on the very next poll and then lose the connection mid-
  // navigation when the Pi finally reboots. Matches the same pattern
  // the SetupWizard's own finalize effect uses.
  useEffect(() => {
    if (appState !== "finalizing") return
    let wentDown = false
    const id = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        if (res.ok && wentDown) {
          setAppState("ready")
        }
      } catch {
        wentDown = true
      }
    }, 2000)
    return () => clearInterval(id)
  }, [appState])

  // Still checking
  if (appState === "loading") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="h-6 w-6 animate-spin rounded-full border-2 border-blue-500 border-t-transparent" />
      </div>
    )
  }

  // Setup is actively running (user refreshed during setup)
  if (appState === "configuring") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-blue-500/20">
            <Loader2 className="h-8 w-8 animate-spin text-blue-400" />
          </div>
          <div>
            <h2 className="text-xl font-semibold text-slate-100">Setting Up Sentry USB</h2>
            <p className="mt-2 text-sm text-slate-400">
              Setup is in progress. The device will reboot several times — this is normal.
            </p>
            <p className="mt-4 text-xs text-slate-600">
              This page will automatically refresh when setup is complete.
              Do not power off the device. This may take 10–20 minutes.
            </p>
          </div>
          <SetupProgress />
        </div>
      </div>
    )
  }

  // Setup finished but the Pi hasn't rebooted yet — stay on this screen
  // until the network drops and recovers so we don't redirect the user
  // into a dashboard that's about to vanish.
  if (appState === "finalizing") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-emerald-500/20">
            <Loader2 className="h-8 w-8 animate-spin text-emerald-400" />
          </div>
          <div>
            <h2 className="text-xl font-semibold text-slate-100">Almost Done!</h2>
            <p className="mt-2 text-sm text-slate-400">
              Setup complete. Rebooting one last time to apply everything — this page will
              redirect automatically once Sentry USB is back online.
            </p>
          </div>
        </div>
      </div>
    )
  }

  // Setup not done — show wizard full screen
  if (appState === "setup") {
    return (
      <div className="min-h-screen bg-slate-950 p-4">
        <SetupWizard onClose={() => setAppState("ready")} />
      </div>
    )
  }

  // Auth check — show login if required and not authenticated
  if (authState === "loading") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="h-6 w-6 animate-spin rounded-full border-2 border-blue-500 border-t-transparent" />
      </div>
    )
  }

  if (authState === "unauthenticated") {
    return (
      <Suspense fallback={<RouteFallback />}>
        <Login onLogin={login} />
      </Suspense>
    )
  }

  return (
    <BrowserRouter>
      <Suspense fallback={<RouteFallback />}>
        <Routes>
          <Route element={<AppShell />}>
            <Route path="/" element={<Dashboard />} />
            <Route path="/viewer" element={<Viewer />} />
            <Route path="/files" element={<Files />} />
            <Route path="/logs" element={<Logs />} />
            <Route path="/drives" element={<Drives />} />
            <Route path="/drives/:id" element={<DriveDetail />} />
            <Route path="/charging" element={<Charging />} />
            <Route path="/charging/:id" element={<ChargeSessionDetail />} />
            <Route path="/fsd" element={<FSDAnalytics />} />
            <Route path="/support" element={<Support />} />
            <Route path="/terminal" element={<Terminal />} />
            <Route path="/community" element={<Community />} />
            <Route path="/notifications" element={<Notifications />} />
            <Route path="/snapshots" element={<Snapshots />} />
            <Route path="/settings" element={<Settings />} />
          </Route>
          {import.meta.env.DEV && (
            <Route path="/preview/charging" element={<PreviewCharging />} />
          )}
        </Routes>
      </Suspense>
    </BrowserRouter>
  )
}

function RouteFallback() {
  return (
    <div className="flex h-screen items-center justify-center bg-slate-950">
      <Loader2 className="h-6 w-6 animate-spin text-blue-400" />
    </div>
  )
}
