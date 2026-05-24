import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { useScrubberSync } from "@/hooks/useScrubberSync"
import type { FsdEvent } from "@/types/drives"

interface DriveMapProps {
  points: [number, number, number, number][]
  fsdEvents?: FsdEvent[]
  source?: string
}

const TILES = {
  dark: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
  streets: "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png",
  satellite:
    "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}",
} as const

type Style = keyof typeof TILES

export function DriveMap({ points, fsdEvents, source }: DriveMapProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const tileRef = useRef<L.TileLayer | null>(null)
  const pulseRef = useRef<L.CircleMarker | null>(null)
  const eventsLayerRef = useRef<L.LayerGroup | null>(null)
  const [style, setStyle] = useState<Style>("dark")
  const [showEvents, setShowEvents] = useState(false)
  const scrubber = useScrubberSync()

  const stroke = source === "tessie" ? "#a78bfa" : "#34d399"

  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current || points.length === 0) return

    const map = L.map(el, { attributionControl: false, zoomControl: true })
    mapRef.current = map
    tileRef.current = L.tileLayer(TILES.dark, { maxZoom: 19 }).addTo(map)

    const latLngs = points.map(([lat, lng]) => L.latLng(lat, lng))
    L.polyline(latLngs, {
      color: stroke,
      weight: 4,
      opacity: 0.95,
      smoothFactor: 1.5,
    }).addTo(map)

    L.circleMarker(latLngs[0], {
      radius: 5,
      color: "#94a3b8",
      weight: 2,
      fillColor: "#1e293b",
      fillOpacity: 1,
    }).addTo(map)
    L.circleMarker(latLngs[latLngs.length - 1], {
      radius: 6,
      color: stroke,
      weight: 2,
      fillColor: stroke,
      fillOpacity: 1,
    }).addTo(map)

    pulseRef.current = L.circleMarker(latLngs[0], {
      radius: 6,
      color: "#ffffff",
      weight: 2,
      fillColor: stroke,
      fillOpacity: 0.9,
    }).addTo(map)

    eventsLayerRef.current = L.layerGroup().addTo(map)
    map.fitBounds(L.latLngBounds(latLngs), { padding: [24, 24], maxZoom: 16 })

    return () => {
      map.remove()
      mapRef.current = null
      tileRef.current = null
      pulseRef.current = null
      eventsLayerRef.current = null
    }
  }, [points, stroke])

  useEffect(() => {
    const map = mapRef.current
    if (!map || !tileRef.current) return
    map.removeLayer(tileRef.current)
    tileRef.current = L.tileLayer(TILES[style], { maxZoom: 19 }).addTo(map)
  }, [style])

  useEffect(() => {
    const layer = eventsLayerRef.current
    if (!layer) return
    layer.clearLayers()
    if (!showEvents || !fsdEvents) return
    for (const ev of fsdEvents) {
      const color = ev.type === "disengagement" ? "#f87171" : "#fbbf24"
      L.circleMarker([ev.lat, ev.lng], {
        radius: 5,
        color,
        weight: 2,
        fillColor: color,
        fillOpacity: 0.8,
      })
        .bindTooltip(ev.type === "disengagement" ? "Disengagement" : "Accel push")
        .addTo(layer)
    }
  }, [showEvents, fsdEvents])

  useEffect(() => {
    const pulse = pulseRef.current
    if (!pulse || points.length === 0) return
    const i = Math.min(points.length - 1, Math.max(0, scrubber.currentIndex))
    pulse.setLatLng(L.latLng(points[i][0], points[i][1]))
  }, [scrubber.currentIndex, points])

  const cycleStyle = () => {
    setStyle((s) => (s === "dark" ? "streets" : s === "streets" ? "satellite" : "dark"))
  }

  return (
    <div className="relative h-80 w-full overflow-hidden rounded-2xl ring-1 ring-inset ring-white/10 sm:h-96">
      <div ref={containerRef} className="absolute inset-0 bg-slate-900" />
      <div className="absolute right-2 top-2 z-[400] flex flex-col gap-1">
        <ControlBtn label={`Map style: ${style}`} onClick={cycleStyle}>
          <Layers className="h-4 w-4" />
        </ControlBtn>
        {fsdEvents && fsdEvents.length > 0 && (
          <button
            type="button"
            onClick={() => setShowEvents((s) => !s)}
            className={cn(
              "rounded-md border border-white/10 bg-slate-900/85 px-2 py-1 text-[10px] font-semibold uppercase tracking-wider backdrop-blur transition-colors",
              showEvents ? "text-emerald-300" : "text-slate-300 hover:text-slate-100",
            )}
            title="Toggle FSD event markers"
          >
            FSD
          </button>
        )}
      </div>
    </div>
  )
}

function ControlBtn({
  label,
  onClick,
  children,
}: {
  label: string
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      className="flex h-8 w-8 items-center justify-center rounded-md border border-white/10 bg-slate-900/85 text-slate-300 backdrop-blur hover:bg-slate-800 hover:text-slate-100"
    >
      {children}
    </button>
  )
}
