import { Wifi, Cable } from "lucide-react"
import { PrefCard, PrefGrid } from "@/components/settings/PrefCard"
import { Row } from "@/components/ui/StatusTile"
import { Pill } from "@/components/ui/Pill"
import CloudPairingSection from "@/components/CloudPairingSection"
import { AwayModeControl } from "@/components/settings/sections/AwayModeControl"
import { BleEnableToggle } from "@/components/settings/sections/BleEnableToggle"
import { BlePairButton } from "@/components/settings/sections/BlePairButton"
import type { PiStatus } from "@/lib/api"

interface Props {
  status: PiStatus | null
}

export function NetworkTab({ status }: Props) {
  const wifiConnected = !!status?.wifi_ssid
  const ethConnected =
    !!status?.ether_speed && status.ether_speed !== "Unknown!"

  return (
    <div className="space-y-2.5">
      {/* Local links sit side-by-side at any width that allows two columns;
          collapse to single column on mobile. Heights can still differ
          (one card may show 1 row, the other 4) but widths stay matched. */}
      <div className="grid items-start gap-2.5 sm:grid-cols-2">
        <PrefCard
          icon={<Wifi className="h-3.5 w-3.5" />}
          halo={wifiConnected ? "accent" : "slate"}
          title="WiFi"
          badge={wifiConnected ? <Pill kind="accent">Connected</Pill> : null}
        >
          {wifiConnected && status ? (
            <>
              <div className="t-md font-semibold">{status.wifi_ssid}</div>
              <Row
                label="IP"
                value={<span className="t-mono">{status.wifi_ip || "—"}</span>}
              />
              {status.wifi_strength && (
                <Row label="Signal" value={status.wifi_strength} />
              )}
            </>
          ) : (
            <p className="t-xs">
              No WiFi configured. Use the Setup Wizard to scan and connect.
            </p>
          )}
        </PrefCard>

        <PrefCard
          icon={<Cable className="h-3.5 w-3.5" />}
          halo={ethConnected ? "accent" : "slate"}
          title="Ethernet"
          badge={
            ethConnected && status ? <Pill kind="accent">{status.ether_speed}</Pill> : null
          }
        >
          {ethConnected && status ? (
            <>
              <Row
                label="IP"
                value={<span className="t-mono">{status.ether_ip || "—"}</span>}
              />
              <Row label="Link" value={status.ether_speed} />
            </>
          ) : (
            <p className="t-xs">No Ethernet link detected.</p>
          )}
        </PrefCard>
      </div>

      {/* Tesla BLE — the car-connection layer. Enable/disable the telemetry
          + keep-awake-nudge features, then pair with the car. Lives here on
          the Car & Network tab alongside the other connectivity surfaces
          (WiFi, Cloud, Away Mode) rather than on the Device tab. */}
      <PrefGrid min={300}>
        <BleEnableToggle />
        <BlePairButton />
      </PrefGrid>

      {/* Away Mode — single card owning both the toggle/duration controls
          and the active-AP details. The AP info used to live in a separate
          card below; it's now a sub-section inside this card so the user
          sees one cohesive thing. */}
      <AwayModeControl />

      {/* SentryCloud spans the full width — it has 4 stat boxes + pairing
          input that need room to breathe. */}
      <CloudPairingSection />
    </div>
  )
}
