import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react"

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
  const playingRef = useRef(playing)
  const speedRef = useRef(playbackSpeed)
  const totalRef = useRef(totalPoints)

  playingRef.current = playing
  speedRef.current = playbackSpeed
  totalRef.current = totalPoints

  useEffect(() => {
    if (!playing) return
    const id = window.setInterval(() => {
      setCurrentIndex((prev) => {
        const next = prev + 1
        if (next >= totalRef.current) {
          setPlaying(false)
          return totalRef.current > 0 ? totalRef.current - 1 : 0
        }
        return next
      })
    }, Math.max(20, Math.floor(100 / speedRef.current)))
    return () => window.clearInterval(id)
  }, [playing, playbackSpeed])

  const setIndex = useCallback((n: number) => {
    setCurrentIndex(() => {
      const total = totalRef.current
      if (total <= 0) return 0
      if (n < 0) return 0
      if (n >= total) return total - 1
      return n
    })
  }, [])

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
