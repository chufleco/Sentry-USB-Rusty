import { Pause, Play } from "lucide-react"
import { cn } from "@/lib/utils"
import { useScrubberSync } from "@/hooks/useScrubberSync"

interface DriveScrubberProps {
  points: [number, number, number, number][]
  startTime: string
}

const SPEEDS = [0.5, 1, 2, 5] as const

export function DriveScrubber({ points, startTime }: DriveScrubberProps) {
  const scrubber = useScrubberSync()
  const max = Math.max(0, points.length - 1)

  const current = points[Math.min(scrubber.currentIndex, max)]
  const label = current ? formatPointTime(current[2], startTime) : "—"

  const togglePlay = () => {
    if (!scrubber.playing && scrubber.currentIndex >= max) {
      scrubber.setIndex(0)
    }
    scrubber.setPlaying(!scrubber.playing)
  }

  return (
    <div className="mt-3 flex items-center gap-3">
      <button
        type="button"
        onClick={togglePlay}
        className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-emerald-500/90 text-slate-950 transition-colors hover:bg-emerald-400"
        aria-label={scrubber.playing ? "Pause" : "Play"}
      >
        {scrubber.playing ? <Pause className="h-4 w-4" /> : <Play className="h-4 w-4" />}
      </button>
      <input
        type="range"
        min={0}
        max={max}
        value={scrubber.currentIndex}
        onChange={(e) => scrubber.setIndex(Number(e.target.value))}
        className="h-1 flex-1 cursor-pointer appearance-none rounded-full bg-white/10 accent-emerald-400"
        aria-label="Drive scrubber"
      />
      <span className="w-20 shrink-0 text-right text-xs tabular-nums text-slate-400">{label}</span>
      <div className="hidden items-center gap-1 sm:flex">
        {SPEEDS.map((s) => (
          <button
            key={s}
            type="button"
            onClick={() => scrubber.setPlaybackSpeed(s)}
            className={cn(
              "rounded px-1.5 py-0.5 text-[10px] font-semibold tabular-nums transition-colors",
              scrubber.playbackSpeed === s
                ? "bg-white/10 text-emerald-300"
                : "text-slate-500 hover:text-slate-300",
            )}
          >
            {s}x
          </button>
        ))}
      </div>
    </div>
  )
}

function formatPointTime(relMs: number, startIso: string): string {
  const baseMs = new Date(startIso).getTime()
  if (!Number.isFinite(baseMs)) return "—"
  const t = new Date(baseMs + relMs)
  if (Number.isNaN(t.getTime())) return "—"
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}
