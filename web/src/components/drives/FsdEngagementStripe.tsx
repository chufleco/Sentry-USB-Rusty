import { useMemo } from "react"
import { useScrubberState } from "@/hooks/useScrubberSync"

interface FsdEngagementStripeProps {
  fsdStates: number[]
}

export function FsdEngagementStripe({ fsdStates }: FsdEngagementStripeProps) {
  const { currentIndex } = useScrubberState()
  const n = fsdStates.length

  const segments = useMemo(() => {
    if (n === 0) return []
    const out: { start: number; end: number; on: boolean }[] = []
    let curStart = 0
    let curOn = fsdStates[0] > 0
    for (let i = 1; i < n; i++) {
      const on = fsdStates[i] > 0
      if (on !== curOn) {
        out.push({ start: curStart, end: i, on: curOn })
        curStart = i
        curOn = on
      }
    }
    out.push({ start: curStart, end: n, on: curOn })
    return out
  }, [fsdStates, n])

  if (n === 0) return null

  const cursorPct = (currentIndex / Math.max(1, n - 1)) * 100

  return (
    <div className="mt-2">
      <div className="relative h-1.5 w-full overflow-hidden rounded-full bg-white/[0.04]">
        {segments.map((seg, i) => {
          const left = (seg.start / n) * 100
          const width = ((seg.end - seg.start) / n) * 100
          return (
            <span
              key={i}
              className={seg.on ? "absolute h-full bg-emerald-400" : "absolute h-full bg-transparent"}
              style={{ left: `${left}%`, width: `${width}%` }}
              aria-hidden
            />
          )
        })}
        <span
          className="pointer-events-none absolute -top-0.5 h-2.5 w-px bg-white"
          style={{ left: `${cursorPct}%` }}
          aria-hidden
        />
      </div>
      <div className="mt-1 text-[10px] uppercase tracking-wider text-slate-500">
        FSD engagement over drive
      </div>
    </div>
  )
}
