/* eslint-disable react-refresh/only-export-components */
import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react"

interface ScrubberState {
  currentIndex: number
  totalPoints: number
  playing: boolean
  playbackSpeed: number
  setIndex: (n: number) => void
  setPlaying: (b: boolean) => void
  setPlaybackSpeed: (s: number) => void
  setTotal: (n: number) => void
}

const ScrubberContext = createContext<ScrubberState | null>(null)

interface ScrubberProviderProps {
  children: React.ReactNode
}

export function ScrubberProvider({ children }: ScrubberProviderProps) {
  const [currentIndex, setCurrentIndex] = useState(0)
  const [totalPoints, setTotalPoints] = useState(0)
  const [playing, setPlaying] = useState(false)
  const [playbackSpeed, setPlaybackSpeed] = useState(1)

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

  const setIndex = useCallback(
    (n: number) => {
      setCurrentIndex(() => {
        if (totalPoints <= 0) return 0
        if (n < 0) return 0
        if (n >= totalPoints) return totalPoints - 1
        return n
      })
    },
    [totalPoints],
  )

  const setTotal = useCallback((n: number) => {
    setTotalPoints(n)
    setCurrentIndex((prev) => (prev >= n ? Math.max(0, n - 1) : prev))
  }, [])

  const value = useMemo<ScrubberState>(
    () => ({
      currentIndex,
      totalPoints,
      playing,
      playbackSpeed,
      setIndex,
      setPlaying,
      setPlaybackSpeed,
      setTotal,
    }),
    [currentIndex, totalPoints, playing, playbackSpeed, setIndex, setTotal],
  )

  return <ScrubberContext.Provider value={value}>{children}</ScrubberContext.Provider>
}

export function useScrubberSync(): ScrubberState {
  const ctx = useContext(ScrubberContext)
  if (!ctx) throw new Error("useScrubberSync must be used inside <ScrubberProvider>")
  return ctx
}
