/* eslint-disable react-refresh/only-export-components */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react"

interface ScrubberStateValue {
  currentIndex: number
  totalPoints: number
  playing: boolean
  playbackSpeed: number
}

interface ScrubberActionsValue {
  setIndex: (n: number) => void
  setPlaying: (b: boolean) => void
  setPlaybackSpeed: (s: number) => void
  setTotal: (n: number) => void
}

// Two contexts so consumers that only need to dispatch (e.g.
// DriveDetailContent calling setTotal once on mount, or DriveChart
// calling setIndex on hover) don't subscribe to currentIndex and thus
// don't re-render on every scrubber tick. Without this split, dragging
// the scrubber re-renders the whole detail page (Speed chart and every
// stat-tile section) at 60fps, which is the source of the drag lag.
const ScrubberStateContext = createContext<ScrubberStateValue | null>(null)
const ScrubberActionsContext = createContext<ScrubberActionsValue | null>(null)

interface ScrubberProviderProps {
  children: React.ReactNode
}

export function ScrubberProvider({ children }: ScrubberProviderProps) {
  const [currentIndex, setCurrentIndex] = useState(0)
  const [totalPoints, setTotalPoints] = useState(0)
  const [playing, setPlaying] = useState(false)
  const [playbackSpeed, setPlaybackSpeed] = useState(1)

  // totalPoints is captured by closure; the playback effect re-runs
  // when totalPoints changes (rare -- once per drive load) and the
  // setIndex setter functional-updates against the current state so
  // it doesn't need a ref. Both action references stay stable across
  // re-renders so the actions context object stays referentially
  // equal and consumers that only use actions never re-render.

  useEffect(() => {
    if (!playing || totalPoints === 0) return
    const tickMs = Math.max(20, Math.floor(100 / playbackSpeed))
    const id = window.setInterval(() => {
      setCurrentIndex((prev) => {
        const next = prev + 1
        if (next >= totalPoints) {
          setPlaying(false)
          return totalPoints - 1
        }
        return next
      })
    }, tickMs)
    return () => window.clearInterval(id)
  }, [playing, playbackSpeed, totalPoints])

  const setIndex = useCallback((n: number) => {
    setCurrentIndex(() => {
      if (n < 0) return 0
      return n
    })
  }, [])

  const setTotal = useCallback((n: number) => {
    setTotalPoints(n)
    setCurrentIndex((prev) => (prev >= n ? Math.max(0, n - 1) : prev))
  }, [])

  const stateValue = useMemo<ScrubberStateValue>(
    () => ({ currentIndex, totalPoints, playing, playbackSpeed }),
    [currentIndex, totalPoints, playing, playbackSpeed],
  )

  // Actions object is built once -- the setters are all stable refs.
  const actionsValue = useMemo<ScrubberActionsValue>(
    () => ({ setIndex, setPlaying, setPlaybackSpeed, setTotal }),
    [setIndex, setTotal],
  )

  return (
    <ScrubberActionsContext.Provider value={actionsValue}>
      <ScrubberStateContext.Provider value={stateValue}>
        {children}
      </ScrubberStateContext.Provider>
    </ScrubberActionsContext.Provider>
  )
}

/** Subscribe to scrubber state. Components using this re-render on
 *  every tick (currentIndex change). Use sparingly. */
export function useScrubberState(): ScrubberStateValue {
  const ctx = useContext(ScrubberStateContext)
  if (!ctx) throw new Error("useScrubberState must be used inside <ScrubberProvider>")
  return ctx
}

/** Stable action setters; using this hook does NOT cause re-renders
 *  on scrubber state changes. Use when you only need to dispatch. */
export function useScrubberActions(): ScrubberActionsValue {
  const ctx = useContext(ScrubberActionsContext)
  if (!ctx) throw new Error("useScrubberActions must be used inside <ScrubberProvider>")
  return ctx
}
