import { useRef } from "react"
import { Pause, Play } from "lucide-react"
import { cn } from "@/lib/utils"
import { useScrubberActions, useScrubberState } from "@/hooks/useScrubberSync"

interface DriveScrubberProps {
  points: [number, number, number, number][]
  startTime: string
}

const SPEEDS = [0.5, 1, 2, 5] as const

export function DriveScrubber({ points, startTime }: DriveScrubberProps) {
  const { currentIndex, playing, playbackSpeed } = useScrubberState()
  const { setIndex, setPlaying, setPlaybackSpeed } = useScrubberActions()
  const max = Math.max(0, points.length - 1)

  // requestAnimationFrame-throttled writes from the slider so dragging
  // never queues more than one state update per frame. Combined with
  // the split state/actions contexts (the parent detail page no longer
  // re-renders on currentIndex change), the thumb tracks the mouse
  // smoothly even with the FSD stripe + map pulse listening.
  const rafRef = useRef<number | null>(null)
  const pendingRef = useRef<number | null>(null)
  const onSliderInput = (val: number) => {
    pendingRef.current = val
    if (rafRef.current === null) {
      rafRef.current = requestAnimationFrame(() => {
        const v = pendingRef.current
        if (v !== null) setIndex(v)
        rafRef.current = null
        pendingRef.current = null
      })
    }
  }

  const baseMs = new Date(startTime).getTime()
  const driveStartLabel =
    points.length > 0 ? formatPointTime(points[0][2], baseMs) : "—"
  const driveEndLabel =
    points.length > 0 ? formatPointTime(points[max][2], baseMs) : "—"
  const currentLabel =
    points.length > 0
      ? formatPointTime(points[Math.min(currentIndex, max)][2], baseMs)
      : "—"

  const cursorPct = max > 0 ? (currentIndex / max) * 100 : 0

  const togglePlay = () => {
    if (!playing && currentIndex >= max) {
      setIndex(0)
    }
    setPlaying(!playing)
  }

  return (
    <div className="mt-3 pb-5">
      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={togglePlay}
          className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-emerald-500/95 text-slate-950 transition-colors hover:bg-emerald-400"
          aria-label={playing ? "Pause" : "Play"}
        >
          {playing ? <Pause className="h-4 w-4" /> : <Play className="h-4 w-4 translate-x-px" />}
        </button>

        <span className="w-16 shrink-0 text-right text-xs tabular-nums text-slate-400">
          {driveStartLabel}
        </span>

        <div className="relative flex-1">
          <input
            type="range"
            min={0}
            max={max}
            value={currentIndex}
            onChange={(e) => onSliderInput(Number(e.target.value))}
            className="h-1 w-full cursor-pointer appearance-none rounded-full bg-white/10 accent-emerald-400"
            aria-label="Drive scrubber"
          />
          {/* Floating current-time label tracks the thumb position. */}
          <div
            className="pointer-events-none absolute -bottom-5 text-[10px] font-semibold tabular-nums text-emerald-300"
            style={{ left: `${cursorPct}%`, transform: "translateX(-50%)" }}
            aria-hidden
          >
            {currentLabel}
          </div>
        </div>

        <span className="w-16 shrink-0 text-left text-xs tabular-nums text-slate-400">
          {driveEndLabel}
        </span>

        <div className="hidden items-center gap-1 sm:flex">
          {SPEEDS.map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => setPlaybackSpeed(s)}
              className={cn(
                "rounded px-1.5 py-0.5 text-[10px] font-semibold tabular-nums transition-colors",
                playbackSpeed === s
                  ? "bg-white/10 text-emerald-300"
                  : "text-slate-500 hover:text-slate-300",
              )}
            >
              {s}x
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}

function formatPointTime(relMs: number, baseMs: number): string {
  if (!Number.isFinite(baseMs)) return "—"
  const t = new Date(baseMs + relMs)
  if (Number.isNaN(t.getTime())) return "—"
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}
