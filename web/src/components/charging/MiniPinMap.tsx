import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"

// A charge session is parked, so its map is a single pin at the
// charger's location — not a route polyline. Mirrors MiniRouteMap's
// lazy-on-visible, interaction-disabled, dark-tile styling so it sits
// next to the drive thumbnails consistently. Renders nothing (an empty
// rounded box) when there are no coordinates.
const DARK_TILES =
  "https://{s}.basemaps.cartocdn.com/dark_nolabels/{z}/{x}/{y}{r}.png"

const PIN_COLOR = "#34d399" // emerald — matches the charging accent

export function MiniPinMap({
  lat,
  lon,
  zoom = 15,
  className = "h-20 w-32",
}: {
  lat: number | null | undefined
  lon: number | null | undefined
  zoom?: number
  className?: string
}) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) {
            setVisible(true)
            io.disconnect()
            break
          }
        }
      },
      { rootMargin: "200px" },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [])

  useEffect(() => {
    if (!visible) return
    const el = containerRef.current
    if (!el || mapRef.current) return
    if (lat == null || lon == null) return

    const map = L.map(el, {
      attributionControl: false,
      zoomControl: false,
      dragging: false,
      scrollWheelZoom: false,
      doubleClickZoom: false,
      touchZoom: false,
      keyboard: false,
      boxZoom: false,
    })
    mapRef.current = map

    L.tileLayer(DARK_TILES, { maxZoom: 18, minZoom: 3 }).addTo(map)
    map.setView([lat, lon], zoom)

    L.circleMarker([lat, lon], {
      radius: 5,
      color: PIN_COLOR,
      weight: 2,
      fillColor: PIN_COLOR,
      fillOpacity: 0.9,
    }).addTo(map)

    return () => {
      map.remove()
      mapRef.current = null
    }
  }, [visible, lat, lon, zoom])

  // `isolate` contains Leaflet's pane z-indexes (up to ~700) within this
  // box so they can't paint over page UI like the tag popover.
  return (
    <div
      ref={containerRef}
      className={`relative isolate shrink-0 overflow-hidden rounded-lg bg-slate-900/60 ring-1 ring-inset ring-white/5 ${className}`}
      role="img"
      aria-label="Charge location"
    />
  )
}
