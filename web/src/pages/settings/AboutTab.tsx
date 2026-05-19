import { Info, Wand2 } from "lucide-react"
import { PrefCard, PrefGrid } from "@/components/settings/PrefCard"
import { Row } from "@/components/ui/StatusTile"
import { Pill } from "@/components/ui/Pill"
import { useVersion } from "@/hooks/useVersion"
import { formatUptime } from "@/lib/utils"
import type { PiStatus } from "@/lib/api"

interface Props {
  status: PiStatus | null
  sbc?: string | null
  hostname?: string | null
  /** Pre-computed uptime in seconds, including the parent's 1s local tick. */
  uptimeSec?: number | null
  onOpenWizard: () => void
}

export function AboutTab({ status, sbc, hostname, uptimeSec, onOpenWizard }: Props) {
  const version = useVersion()
  const uptime = uptimeSec ?? (status ? parseFloat(status.uptime) : null)

  return (
    <PrefGrid min={300}>
      <PrefCard icon={<Info className="h-3.5 w-3.5" />} halo="slate" title="System">
        <Row label="Version" value={<span className="t-mono">{version ?? "…"}</span>} />
        <Row label="Channel" value={<Pill kind="slate">Stable</Pill>} />
        {sbc && <Row label="SBC" value={sbc} />}
        {hostname && (
          <Row label="Hostname" value={<span className="t-mono">{hostname}</span>} />
        )}
        {uptime != null && uptime > 0 && (
          <Row label="Uptime" value={formatUptime(uptime)} />
        )}
      </PrefCard>

      <PrefCard
        icon={<Wand2 className="h-3.5 w-3.5" />}
        halo="accent"
        title="Setup Wizard"
      >
        <p className="t-xs">
          Re-run the first-time setup wizard to reconfigure WiFi, drives, time zones and units.
          Safe to run any time — your existing config is the starting point.
        </p>
        <button
          onClick={onOpenWizard}
          className="self-start rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
        >
          Launch Wizard
        </button>
        <div className="tile-divider" />
        <p className="section-label">Resources</p>
        <div className="flex flex-col gap-1">
          <a
            href="https://github.com/Sentry-Six/Sentry-USB-Rusty"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            GitHub repository ↗
          </a>
          <a
            href="https://discord.gg/9QZEzVwdnt"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-violet-400 hover:text-violet-300"
          >
            Discord community ↗
          </a>
        </div>
      </PrefCard>
    </PrefGrid>
  )
}
