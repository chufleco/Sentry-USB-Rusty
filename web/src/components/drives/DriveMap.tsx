import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { Layers } from "lucide-react"
import { useScrubberState } from "@/hooks/useScrubberSync"
import type { FsdEvent } from "@/types/drives"

interface DriveMapProps {
  points: [number, number, number, number][]
  fsdStates?: number[]
  fsdEvents?: FsdEvent[]
  showEvents?: boolean
  source?: string
  // ISO start_time of the drive — needed to convert each point's
  // relative-ms field into an absolute clock time for the playback
  // info card. Without it the card would show offsets, not times.
  startTime?: string
  // True when the user's DRIVE_MAP_UNIT === "km". Controls the speed
  // unit displayed in the playback card.
  metric?: boolean
  // Battery percentage at the drive's start / end (drive-level fields
  // from telemetry aggregation — we don't have per-point battery).
  // When both are present, the playback card shows a linearly
  // interpolated battery % at the current scrubber index. When either
  // is missing, the battery row is omitted from the card.
  batteryStart?: number
  batteryEnd?: number
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

// SVG markup for the steering-wheel glyph in the playback info card.
// Three spokes (top, lower-left, lower-right) sketch a Tesla-yoke look
// in a 24-viewBox stroke-only icon — colour comes from CSS currentColor.
const WHEEL_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" width="14" height="14">' +
  '<circle cx="12" cy="12" r="9"/>' +
  '<circle cx="12" cy="12" r="2"/>' +
  '<line x1="12" y1="3" x2="12" y2="10"/>' +
  '<line x1="4.5" y1="16.5" x2="10" y2="13"/>' +
  '<line x1="19.5" y1="16.5" x2="14" y2="13"/>' +
  "</svg>"

const MPS_TO_MPH = 2.23694
const MPS_TO_KPH = 3.6

// Inline-SVG battery icon. Outer rectangle with a small terminal nub
// on the right; the inner fill rect's width is set inline at call
// time from the current battery percentage so the visual gauge tracks
// the playback. `fill="currentColor"` so the CSS pill colour controls
// the whole glyph.
function batterySVG(pct: number): string {
  // Inner fill spans x=3..x=18 (15 units wide max) and is proportional
  // to pct. clamp so out-of-range readings can't blow past the bounds.
  const fillW = Math.max(0, Math.min(15, (pct / 100) * 15))
  return (
    '<svg viewBox="0 0 22 12" width="18" height="10" fill="none" stroke="currentColor" stroke-width="1.2">' +
    '<rect x="1" y="1" width="18" height="10" rx="2"/>' +
    `<rect x="3" y="3" width="${fillW.toFixed(2)}" height="6" rx="1" fill="currentColor" stroke="none"/>` +
    '<rect x="20" y="4" width="2" height="4" rx="0.5" fill="currentColor" stroke="none"/>' +
    "</svg>"
  )
}

// Build the HTML body of the playback tooltip for the given scrubber
// index. Returns a string so Leaflet's `tooltip.setContent` can swap
// it on every tick without paying React-reconciliation cost.
function renderPlaybackHTML(
  pt: [number, number, number, number] | undefined,
  fsd: number | undefined,
  baseMs: number,
  metric: boolean,
  battery: number | undefined,
): string {
  if (!pt) return ""
  const speedMps = pt[3] ?? 0
  const speed = Math.round(metric ? speedMps * MPS_TO_KPH : speedMps * MPS_TO_MPH)
  const unit = metric ? "km/h" : "mph"
  const time = Number.isFinite(baseMs)
    ? new Date(baseMs + pt[2]).toLocaleTimeString([], {
        hour: "numeric",
        minute: "2-digit",
        second: "2-digit",
      })
    : ""
  const isFsd = (fsd ?? 0) > 0
  const wheelClass = isFsd
    ? "playback-info__wheel playback-info__wheel--fsd"
    : "playback-info__wheel"
  const wheelTitle = isFsd ? "FSD engaged" : "Manual driving"
  const batteryHtml =
    battery !== undefined && Number.isFinite(battery)
      ? `<span class="playback-info__battery" aria-label="Battery ${Math.round(battery)}%">` +
        `${batterySVG(battery)}${Math.round(battery)}%</span>`
      : ""
  return (
    `<div class="playback-info__time">${time}</div>` +
    `<div class="playback-info__row">` +
    `<span class="playback-info__speed">${speed} ${unit}</span>` +
    `<span class="${wheelClass}" title="${wheelTitle}" aria-label="${wheelTitle}">${WHEEL_SVG}</span>` +
    batteryHtml +
    `</div>`
  )
}

// Linear interpolation between drive-start and drive-end battery
// percent based on the scrubber's progress through the route. Returns
// undefined when either end is missing so the caller can omit the
// battery row from the tooltip rather than show "—%".
function interpolateBattery(
  start: number | undefined,
  end: number | undefined,
  idx: number,
  total: number,
): number | undefined {
  if (start === undefined || end === undefined) return undefined
  if (total <= 1) return start
  const t = Math.max(0, Math.min(1, idx / (total - 1)))
  return start + (end - start) * t
}

export function DriveMap({
  points,
  fsdStates,
  fsdEvents,
  showEvents = true,
  source,
  startTime,
  metric = false,
  batteryStart,
  batteryEnd,
}: DriveMapProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const tileRef = useRef<L.TileLayer | null>(null)
  const pulseRef = useRef<L.Marker | null>(null)
  const eventsLayerRef = useRef<L.LayerGroup | null>(null)
  const [style, setStyle] = useState<Style>("dark")
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
    // The pulse marker MUST be interactive so the bound tooltip can
    // open via openTooltip() — Leaflet refuses to open tooltips on
    // non-interactive markers. We still set keyboard:false so it
    // doesn't capture tab focus.
    pulseRef.current = L.marker(latLngs[0], {
      icon: pulseMarkerIcon(),
      interactive: true,
      keyboard: false,
      zIndexOffset: 1000,
    }).addTo(map)

    // Playback info card — permanent tooltip floating next to the
    // pulse marker. Bound once here, content swapped on each scrubber
    // tick by the dedicated effect below. Position "right" so the
    // card sits beside the pulse rather than covering it; Leaflet
    // auto-flips when it would go off the map edge.
    const baseMs = startTime ? new Date(startTime).getTime() : NaN
    const initialBattery = interpolateBattery(
      batteryStart,
      batteryEnd,
      0,
      points.length,
    )
    const initialHtml = renderPlaybackHTML(
      points[0],
      fsdStates?.[0],
      baseMs,
      metric,
      initialBattery,
    )
    pulseRef.current
      .bindTooltip(initialHtml, {
        permanent: true,
        direction: "right",
        offset: [10, 0],
        className: "playback-info-tooltip",
        opacity: 1,
      })
      .openTooltip()

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
    // Refresh the playback info card to match the new position.
    // setContent on an attached Leaflet tooltip patches its innerHTML
    // in place — no React render, no marker rebind, no map redraw.
    const tooltip = pulse.getTooltip()
    if (tooltip) {
      const baseMs = startTime ? new Date(startTime).getTime() : NaN
      const battery = interpolateBattery(
        batteryStart,
        batteryEnd,
        i,
        points.length,
      )
      tooltip.setContent(
        renderPlaybackHTML(
          points[i],
          fsdStates?.[i],
          baseMs,
          metric,
          battery,
        ),
      )
    }
  }, [currentIndex, points, fsdStates, startTime, metric, batteryStart, batteryEnd])

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
