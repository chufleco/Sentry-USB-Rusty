import { useEffect, useState, lazy, Suspense } from "react"
import { useSearchParams } from "react-router-dom"
import {
  RefreshCw,
  Stethoscope,
  Gauge,
  Settings as SettingsIcon,
  Unplug,
  RotateCcw,
  Loader2,
  Download,
} from "lucide-react"
import { api } from "@/lib/api"
import { HeaderStrip } from "@/components/settings/HeaderStrip"
import { ActionsRail, type ActionChipProps } from "@/components/settings/ActionsRail"
import { TabBar } from "@/components/ui/TabBar"
import { useVersion } from "@/hooks/useVersion"
import type { RawConfigEntry } from "@/components/settings/sections/RawConfigEditor"
import type { PiStatus } from "@/lib/api"

// Lazy tab chunks — only the active tab pays for its bundle. Visiting
// /settings without clicking a tab pulls just the shell + Device
// (default). Each tab module exports a named component, so wrap with
// `.then(m => ({ default: m.X }))` to satisfy React.lazy's
// default-export contract.
const DeviceTab = lazy(() => import("@/pages/settings/DeviceTab").then(m => ({ default: m.DeviceTab })))
const NetworkTab = lazy(() => import("@/pages/settings/NetworkTab").then(m => ({ default: m.NetworkTab })))
const UpdatesTab = lazy(() => import("@/pages/settings/UpdatesTab").then(m => ({ default: m.UpdatesTab })))
const BackupsTab = lazy(() => import("@/pages/settings/BackupsTab").then(m => ({ default: m.BackupsTab })))
const NotificationsTab = lazy(() => import("@/pages/settings/NotificationsTab").then(m => ({ default: m.NotificationsTab })))
const PrivacyTab = lazy(() => import("@/pages/settings/PrivacyTab").then(m => ({ default: m.PrivacyTab })))
const AboutTab = lazy(() => import("@/pages/settings/AboutTab").then(m => ({ default: m.AboutTab })))

// Modals are only mounted while open — defer their bundles entirely
// until the user opens them. SetupWizard alone pulls in several
// step components + the BLE pairing flow.
const SetupWizard = lazy(() => import("@/components/setup/SetupWizard").then(m => ({ default: m.SetupWizard })))
const RawConfigEditor = lazy(() => import("@/components/settings/sections/RawConfigEditor").then(m => ({ default: m.RawConfigEditor })))
const HealthCheckModal = lazy(() => import("@/components/settings/sections/HealthCheckModal").then(m => ({ default: m.HealthCheckModal })))
const SpeedTestModal = lazy(() => import("@/components/settings/sections/SpeedTestModal").then(m => ({ default: m.SpeedTestModal })))

const TABS = [
  "Device",
  "Network",
  "Updates",
  "Backups",
  "Notifications",
  "Privacy",
  "About",
] as const
type TabName = (typeof TABS)[number]

function isTab(s: string | null): s is TabName {
  return !!s && (TABS as readonly string[]).includes(s)
}

export default function Settings() {
  const [params, setParams] = useSearchParams()
  const activeTab: TabName = isTab(params.get("tab")) ? (params.get("tab") as TabName) : "Device"

  const [status, setStatus] = useState<PiStatus | null>(null)
  const [piConfig, setPiConfig] = useState<{
    uses_ble?: string
    SENTRYUSB_HOSTNAME?: string
  } | null>(null)
  const [confirmReboot, setConfirmReboot] = useState(false)
  const [drivesConnected, setDrivesConnected] = useState<boolean | null>(null)
  // Local 1s tick so the header strip uptime advances between status polls
  // (status itself only refreshes every 4s — too jumpy for a wall clock).
  const [tickOffset, setTickOffset] = useState(0)

  // Modal state
  const [wizardOpen, setWizardOpen] = useState(false)
  const [wizardInitialData, setWizardInitialData] = useState<
    Record<string, string> | undefined
  >(undefined)
  const [rawConfigOpen, setRawConfigOpen] = useState(false)
  const [rawConfig, setRawConfig] = useState<Record<string, RawConfigEntry> | null>(null)
  const [healthOpen, setHealthOpen] = useState(false)
  const [speedOpen, setSpeedOpen] = useState(false)

  // Version is included in the Export Config file header so the receiver
  // knows which schema produced it. HeaderStrip fetches its own copy via
  // the same hook — the underlying request is cheap and the hook caches
  // nothing across mounts, but /api/system/version itself is a static
  // string so duplicate calls are harmless.
  const version = useVersion()

  // Status poll (drives the actions rail USB toggle + header strip uptime)
  useEffect(() => {
    let mounted = true
    async function poll() {
      try {
        const data = await api.getStatus()
        if (mounted) {
          setStatus(data)
          setDrivesConnected(data.drives_active === "yes")
          setTickOffset(0) // reset local tick when we get a fresh server value
        }
      } catch {
        /* ignore */
      }
    }
    poll()
    const id = setInterval(poll, 4000)
    const tickId = setInterval(() => setTickOffset((t) => t + 1), 1000)
    return () => {
      mounted = false
      clearInterval(id)
      clearInterval(tickId)
    }
  }, [])

  // Pi config (uses_ble, SENTRYUSB_HOSTNAME). SBC model now comes from the
  // status payload (which already detects Pi 5 reliably) — the previous
  // rtc-status flag only flipped to is_pi5=true when a battery was present,
  // so a Pi 5 without an RTC battery was mis-labelled as "Pi 4 / earlier".
  useEffect(() => {
    fetch("/api/config")
      .then((r) => r.json())
      .then((data) => setPiConfig(data))
      .catch(() => {})
  }, [])

  const sbc = status?.sbc_model || null
  // /api/config returns each key as { value, active } OR as a raw string,
  // depending on shape — handle both.
  const hostnameEntry = piConfig?.SENTRYUSB_HOSTNAME as
    | { value?: string; active?: boolean }
    | string
    | undefined
  const hostname =
    typeof hostnameEntry === "string"
      ? hostnameEntry
      : hostnameEntry?.active
      ? hostnameEntry.value || null
      : null

  function setTab(next: TabName) {
    const p = new URLSearchParams(params)
    p.set("tab", next)
    setParams(p, { replace: true })
  }

  async function handleReboot(): Promise<string | void> {
    // First press arms the confirm; the parent re-renders the chip's label
    // ("Restart Pi" → "Confirm Restart"). Returning "confirm" tells the chip
    // not to flash a success state — the label change is the feedback.
    if (!confirmReboot) {
      setConfirmReboot(true)
      setTimeout(() => setConfirmReboot(false), 10000)
      return "confirm"
    }
    const res = await fetch("/api/system/reboot", { method: "POST" })
    setConfirmReboot(false)
    if (!res.ok) throw new Error("Reboot failed")
    return "Rebooting…"
  }

  async function handleToggleDrives(): Promise<string> {
    const res = await fetch("/api/system/toggle-drives", { method: "POST" })
    if (!res.ok) throw new Error("Toggle failed")
    // Eagerly refresh status so the chip label updates to the new state
    // ("USB · Connected" / "USB · Disconnected") on the next render.
    try {
      const data = await api.getStatus()
      setDrivesConnected(data.drives_active === "yes")
    } catch {
      /* non-critical */
    }
    return "Toggled"
  }

  async function handleArchiveSync(): Promise<string> {
    const res = await fetch("/api/system/trigger-sync", { method: "POST" })
    if (!res.ok) throw new Error("Sync failed")
    return "Triggered"
  }

  async function handleOpenRawConfig() {
    try {
      const res = await fetch("/api/setup/config")
      if (!res.ok) return
      const data = await res.json()
      setRawConfig(data)
      setRawConfigOpen(true)
    } catch {
      /* ignore */
    }
  }

  // Export the device's full configuration as a bash-sourceable .conf
  // file. Matches the format the old Go version produced so the file
  // round-trips between versions: active settings become `export KEY='value'`
  // lines, defaults become `# export KEY='value'` so the user can see what
  // they didn't change. Rusty-only Web-UI preferences (the JSON kv-store
  // at /mutable/.sentryusb_preferences.json — new in the Rust rewrite) are
  // appended at the bottom as `# preference:` comment lines: kept here for
  // export completeness without polluting the bash namespace if the file
  // is ever sourced. Single quotes inside values are escaped via the
  // standard '\'' trick so the file stays valid bash even for passwords
  // containing apostrophes.
  async function handleExportConfig(): Promise<string> {
    const [configRes, prefsRes] = await Promise.all([
      fetch("/api/setup/config"),
      fetch("/api/config/preference"),
    ])
    if (!configRes.ok) throw new Error("Failed to read config")
    const config = (await configRes.json()) as Record<
      string,
      { value: string; active: boolean }
    >
    const prefs = prefsRes.ok
      ? ((await prefsRes.json()) as Record<string, unknown>)
      : {}

    const now = new Date().toISOString()
    const ver = version || "unknown"
    const host = hostname || "sentryusb"
    const escape = (s: string) => (s ?? "").replace(/'/g, "'\\''")

    let content = ""
    content += `# sentryusb.conf — exported from Sentry USB UI\n`
    content += `# Exported:  ${now}\n`
    content += `# Hostname:  ${host}\n`
    content += `# Version:   ${ver}\n`
    content += `#\n`
    content += `# This file is bash-sourceable. Active settings are 'export' lines;\n`
    content += `# inactive/default values are commented out for reference.\n`
    content += `\n`
    content += `# === Setup configuration ===\n`

    // Sort for stable, diff-friendly output across exports.
    const keys = Object.keys(config).sort()
    for (const k of keys) {
      const e = config[k]
      const v = escape(e.value ?? "")
      if (e.active) {
        content += `export ${k}='${v}'\n`
      } else {
        content += `# export ${k}='${v}'\n`
      }
    }

    // Web-UI preferences (new in the Rusty rewrite). The Go version had
    // no equivalent concept — preferences were either bash exports in the
    // conf file or hard-coded defaults. Now things like keep-awake mode,
    // backup location, notification toggles, and community feature flags
    // live in a key-value JSON store; include them here as comments so a
    // human reading this file can see the complete configuration state.
    const prefKeys = Object.keys(prefs).sort()
    if (prefKeys.length > 0) {
      content += `\n`
      content += `# === Web UI preferences (Sentry USB Rusty) ===\n`
      content += `# Managed via the web UI; stored in /mutable/.sentryusb_preferences.json.\n`
      content += `# Listed here for export completeness — these are NOT sourced by bash.\n`
      for (const k of prefKeys) {
        const v = prefs[k]
        // JSON.stringify keeps the type info (string vs boolean vs number)
        // visible in the export, which matters because the same key can be
        // either "enabled"/"disabled" strings or true/false booleans
        // depending on which UI screen wrote it.
        content += `# preference: ${k} = ${JSON.stringify(v)}\n`
      }
    }

    const blob = new Blob([content], { type: "text/plain" })
    const url = URL.createObjectURL(blob)
    const a = document.createElement("a")
    a.href = url
    a.download = "sentryusb.conf"
    a.click()
    URL.revokeObjectURL(url)
    return "Downloaded"
  }

  async function handleOpenWizard() {
    try {
      const res = await fetch("/api/setup/config")
      if (res.ok) {
        const data = await res.json()
        const flat: Record<string, string> = {}
        for (const [k, v] of Object.entries(data)) {
          const entry = v as { value: string; active: boolean }
          if (entry.active) flat[k] = entry.value
        }
        setWizardInitialData(flat)
      }
    } catch {
      /* ignore */
    }
    setWizardOpen(true)
  }

  const actions: ActionChipProps[] = [
    {
      icon: RefreshCw,
      label: "Archive Sync",
      onClick: handleArchiveSync,
    },
    {
      icon: Stethoscope,
      label: "Health Check",
      onClick: () => setHealthOpen(true),
    },
    {
      icon: Gauge,
      label: "Speed Test",
      onClick: () => setSpeedOpen(true),
    },
    {
      icon: SettingsIcon,
      label: "Raw Config",
      onClick: handleOpenRawConfig,
    },
    {
      icon: Download,
      label: "Export Config",
      onClick: handleExportConfig,
    },
    {
      icon: Unplug,
      label:
        drivesConnected === null
          ? "Toggle USB"
          : drivesConnected
          ? "USB · Connected"
          : "USB · Disconnected",
      onClick: handleToggleDrives,
    },
  ]
  const dangerActions: ActionChipProps[] = [
    {
      icon: RotateCcw,
      label: confirmReboot ? "Confirm Restart" : "Restart Pi",
      variant: "danger",
      onClick: handleReboot,
    },
  ]

  const uptimeSec = status ? parseFloat(status.uptime) + tickOffset : null

  // ⚠️ Mobile / tab-bar — switch to scrollable variant under 640px.
  const [isMobile, setIsMobile] = useState(
    typeof window !== "undefined" && window.innerWidth < 640
  )
  useEffect(() => {
    const onResize = () => setIsMobile(window.innerWidth < 640)
    window.addEventListener("resize", onResize)
    return () => window.removeEventListener("resize", onResize)
  }, [])

  return (
    <div className="space-y-3">
      <HeaderStrip
        hostname={hostname}
        sbc={sbc}
        uptimeSec={uptimeSec}
        onOpenWizard={handleOpenWizard}
      />

      <ActionsRail actions={actions} danger={dangerActions} />

      <TabBar tabs={TABS} active={activeTab} onSelect={setTab} scrollable={isMobile} />

      <Suspense fallback={<TabFallback />}>
        {activeTab === "Device" && <DeviceTab />}
        {activeTab === "Network" && <NetworkTab status={status} />}
        {activeTab === "Updates" && <UpdatesTab />}
        {activeTab === "Backups" && <BackupsTab onOpenRawConfig={handleOpenRawConfig} />}
        {activeTab === "Notifications" && <NotificationsTab />}
        {activeTab === "Privacy" && <PrivacyTab />}
        {activeTab === "About" && (
          <AboutTab
            status={status}
            sbc={sbc}
            hostname={hostname}
            uptimeSec={uptimeSec}
            onOpenWizard={handleOpenWizard}
          />
        )}
      </Suspense>

      {/* Modals — each wrapped in its own Suspense so a slow chunk for
          one doesn't block the rest of the page. */}
      {wizardOpen && (
        <Suspense fallback={null}>
          <SetupWizard
            initialData={wizardInitialData}
            onClose={() => {
              setWizardOpen(false)
              setWizardInitialData(undefined)
            }}
          />
        </Suspense>
      )}
      {rawConfigOpen && rawConfig && (
        <Suspense fallback={null}>
          <RawConfigEditor
            config={rawConfig}
            onClose={() => {
              setRawConfigOpen(false)
              setRawConfig(null)
            }}
          />
        </Suspense>
      )}
      {healthOpen && (
        <Suspense fallback={null}>
          <HealthCheckModal onClose={() => setHealthOpen(false)} />
        </Suspense>
      )}
      {speedOpen && (
        <Suspense fallback={null}>
          <SpeedTestModal onClose={() => setSpeedOpen(false)} />
        </Suspense>
      )}
    </div>
  )
}

function TabFallback() {
  return (
    <div className="flex h-32 items-center justify-center">
      <Loader2 className="h-5 w-5 animate-spin text-slate-500" />
    </div>
  )
}
