import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { Layers } from "lucide-react"
import { cn } from "@/lib/utils"
import { useScrubberState } from "@/hooks/useScrubberSync"
import type { FsdEvent } from "@/types/drives"

interface DriveMapProps {
  points: [number, number, number, number][]
  fsdStates?: number[]
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

// Colors for the route polyline segments.
// FSD-engaged: emerald accent. Manual: indigo-blue (echo of the old design
// for instant familiarity). When fsdStates is unavailable, the route falls
// back to a single emerald polyline OR a violet polyline for Tessie-source
// drives (matches the existing source badge convention).
const COLOR_FSD = "#34d399"
const COLOR_MANUAL = "#3b82f6"
const COLOR_TESSIE = "#a78bfa"

function startMarkerIcon() {
  return L.divIcon({
    className: "",
    html: '<div style="width:12px;height:12px;border-radius:50%;background:#34d399;border:2px solid #fff;box-shadow:0 0 4px rgba(0,0,0,0.4)"></div>',
    iconSize: [12, 12],
    iconAnchor: [6, 6],
  })
}

function endMarkerIcon() {
  return L.divIcon({
    className: "",
    html: '<div style="width:12px;height:12px;border-radius:50%;background:#ef4444;border:2px solid #fff;box-shadow:0 0 4px rgba(0,0,0,0.4)"></div>',
    iconSize: [12, 12],
    iconAnchor: [6, 6],
  })
}

function pulseMarkerIcon() {
  return L.divIcon({
    className: "",
    html: '<div style="width:14px;height:14px;border-radius:50%;background:#34d399;border:2px solid #fff;box-shadow:0 0 8px rgba(52,211,153,0.7)"></div>',
    iconSize: [14, 14],
    iconAnchor: [7, 7],
  })
}

function fsdEventIcon(kind: "disengagement" | "accel_push") {
  const isDisengage = kind === "disengagement"
  const color = isDisengage ? "#ef4444" : "#f59e0b"
  const label = isDisengage ? "D" : "A"
  return L.divIcon({
    className: "",
    html: `<div style="width:18px;height:18px;border-radius:50%;background:${color};border:2px solid #fff;display:flex;align-items:center;justify-content:center;font-size:10px;font-weight:700;color:#fff;line-height:1;box-shadow:0 0 4px rgba(0,0,0,0.5)">${label}</div>`,
    iconSize: [18, 18],
    iconAnchor: [9, 9],
  })
}

export function DriveMap({ points, fsdStates, fsdEvents, source }: DriveMapProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const tileRef = useRef<L.TileLayer | null>(null)
  const pulseRef = useRef<L.Marker | null>(null)
  const eventsLayerRef = useRef<L.LayerGroup | null>(null)
  const [style, setStyle] = useState<Style>("dark")
  const [showEvents, setShowEvents] = useState(true)
  const { currentIndex } = useScrubberState()

  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current || points.length === 0) return

    // preferCanvas keeps the polyline(s) on a single 2D canvas, which
    // re-projects much faster than the default SVG renderer on zoom for
    // routes with thousands of points. The pulse marker stays as a DOM
    // divIcon so its scrubber-driven setLatLng() moves via CSS transform
    // without triggering a canvas redraw.
    const map = L.map(el, {
      attributionControl: false,
      zoomControl: true,
      preferCanvas: true,
    })
    mapRef.current = map
    tileRef.current = L.tileLayer(TILES.dark, { maxZoom: 19 }).addTo(map)

    const latLngs = points.map(([lat, lng]) => L.latLng(lat, lng))

    // Segment the polyline by FSD state when fsdStates is parallel to
    // points. Each contiguous run of the same engagement state becomes
    // one polyline; adjacent segments overlap by one point so there's
    // no visible gap at the transition. Falls back to a single-color
    // polyline (emerald for SEI, violet for Tessie-source) when
    // fsdStates is missing or length-mismatched.
    const hasFsdSegments =
      fsdStates !== undefined && fsdStates.length === points.length
    if (hasFsdSegments) {
      let segStart = 0
      for (let i = 1; i <= points.length; i++) {
        const prevEngaged = fsdStates[i - 1] > 0
        const curEngaged = i < points.length ? fsdStates[i] > 0 : !prevEngaged
        if (curEngaged !== prevEngaged || i === points.length) {
          const segPts = latLngs.slice(segStart, i)
          if (segPts.length >= 2) {
            L.polyline(segPts, {
              color: prevEngaged ? COLOR_FSD : COLOR_MANUAL,
              weight: 4,
              opacity: 1,
              smoothFactor: 1.2,
            }).addTo(map)
          }
          segStart = Math.max(i - 1, 0)
        }
      }
    } else {
      const stroke = source === "tessie" ? COLOR_TESSIE : COLOR_FSD
      L.polyline(latLngs, {
        color: stroke,
        weight: 4,
        opacity: 1,
        smoothFactor: 1.2,
      }).addTo(map)
    }

    // Start / end / pulse all use DOM markers (divIcon) so the pulse
    // marker can move on every scrubber tick without redrawing the
    // canvas-rendered polylines.
    L.marker(latLngs[0], { icon: startMarkerIcon(), interactive: false }).addTo(map)
    L.marker(latLngs[latLngs.length - 1], { icon: endMarkerIcon(), interactive: false }).addTo(map)
    pulseRef.current = L.marker(latLngs[0], {
      icon: pulseMarkerIcon(),
      interactive: false,
      keyboard: false,
      zIndexOffset: 1000,
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
  }, [points, fsdStates, source])

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
      const title = ev.type === "disengagement" ? "FSD Disengagement" : "Accel Push"
      L.marker([ev.lat, ev.lng], {
        icon: fsdEventIcon(ev.type),
        title,
        riseOnHover: true,
      })
        .bindTooltip(title, { direction: "top", offset: [0, -10] })
        .addTo(layer)
    }
  }, [showEvents, fsdEvents])

  useEffect(() => {
    const pulse = pulseRef.current
    if (!pulse || points.length === 0) return
    const i = Math.min(points.length - 1, Math.max(0, currentIndex))
    pulse.setLatLng(L.latLng(points[i][0], points[i][1]))
  }, [currentIndex, points])

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
            title={
              showEvents
                ? "Hide FSD event markers"
                : "Show FSD event markers"
            }
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
