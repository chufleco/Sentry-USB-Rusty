import { useLayoutEffect, useRef, type ReactNode } from "react"
import { cn } from "@/lib/utils"
import type { Halo } from "@/components/ui/StatusTile"

/**
 * Configures the "feature unavailable" overlay drawn over the card body when
 * `disabled` is set on a PrefCard. The header (icon + title + badge) keeps
 * rendering normally — only the body is blurred and inerted, so users can
 * still tell which card they're looking at.
 */
export interface DisabledConfig {
  /** Centered text explaining what the user needs to enable. */
  reason: string
  /** Optional "go enable this" affordance. */
  cta?: {
    label: string
    onClick?: () => void
    href?: string
  }
}

interface PrefCardProps {
  icon: ReactNode
  halo?: Halo
  title: ReactNode
  badge?: ReactNode
  footer?: ReactNode
  className?: string
  /**
   * When set, the card body is rendered behind a blurred, non-interactive
   * overlay with the supplied reason + optional CTA. Children stay mounted
   * (no state loss on re-enable). The body wrapper carries the `inert`
   * attribute, which removes it from the focus order and from assistive
   * tech — `pointer-events: none` alone wouldn't block keyboard Tab.
   */
  disabled?: DisabledConfig
  children: ReactNode
}

export function PrefCard({
  icon,
  halo = "slate",
  title,
  badge,
  footer,
  className,
  disabled,
  children,
}: PrefCardProps) {
  return (
    <div
      className={cn("glass-card overflow-hidden", className)}
      aria-disabled={disabled ? true : undefined}
      data-disabled={disabled ? "true" : undefined}
    >
      <div className="flex items-center gap-2.5 border-b border-white/5 px-3.5 py-2.5">
        <span
          className={cn("halo-" + halo, "inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg")}
        >
          {icon}
        </span>
        <span className="t-md font-semibold">{title}</span>
        {badge && <span className="ml-auto">{badge}</span>}
      </div>
      {disabled ? (
        <div className="relative">
          {/* Keep the real controls mounted (no state loss on re-enable)
              but push them fully behind a frosted scrim so their text
              can't bleed through and collide with the centered message —
              the previous blur-only treatment left the body legible and
              looked like a rendering glitch. */}
          <div
            inert
            aria-hidden
            className="flex flex-col gap-2.5 p-3.5 blur-[3px] opacity-20 select-none"
          >
            {children}
          </div>
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-2.5 bg-gradient-to-b from-slate-900/75 to-slate-950/85 p-4 text-center backdrop-blur-[1px]">
            <p className="max-w-[30ch] text-xs leading-relaxed text-slate-300">{disabled.reason}</p>
            {disabled.cta && <DisabledCta cta={disabled.cta} />}
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-2.5 p-3.5">{children}</div>
      )}
      {footer && (
        <div className="border-t border-white/5 px-3.5 py-2.5">{footer}</div>
      )}
    </div>
  )
}

function DisabledCta({ cta }: { cta: NonNullable<DisabledConfig["cta"]> }) {
  const cls =
    "rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
  if (cta.href) {
    return (
      <a href={cta.href} className={cls}>
        {cta.label}
      </a>
    )
  }
  return (
    <button type="button" onClick={cta.onClick} className={cls}>
      {cta.label}
    </button>
  )
}

/** Must match the `gap-2.5` (0.625rem) used between/inside PrefGrid columns. */
const GRID_GAP_PX = 10

/**
 * Greedily pack `heights` (kept in order) into the fewest columns where no
 * column exceeds `cap`; null when more than `max` columns would be needed.
 */
function packWithin(heights: number[], cap: number, max: number): number[] | null {
  const runs: number[] = []
  let count = 0
  let h = 0
  for (const x of heights) {
    if (count > 0 && h + GRID_GAP_PX + x > cap) {
      runs.push(count)
      count = 1
      h = x
    } else {
      count += 1
      h += count === 1 ? x : GRID_GAP_PX + x
    }
  }
  if (count > 0) runs.push(count)
  return runs.length <= max ? runs : null
}

/**
 * Split items into at most `n` contiguous runs (one per column), minimising
 * the tallest column — the same balancing CSS multi-column performs. Binary
 * search on the smallest feasible column height. Padded with empty trailing
 * columns so the result always has `n` entries.
 */
function balancedRuns(heights: number[], n: number): number[] {
  let lo = Math.max(0, ...heights)
  let hi = heights.reduce((a, b) => a + b, 0) + GRID_GAP_PX * Math.max(0, heights.length - 1)
  let best = packWithin(heights, hi, n) ?? [heights.length]
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2)
    const runs = packWithin(heights, mid, n)
    if (runs) {
      best = runs
      hi = mid
    } else {
      lo = mid + 1
    }
  }
  while (best.length < n) best.push(0)
  return best
}

/**
 * Card wrapper used by every settings tab. Masonry that reproduces the CSS
 * multi-column layout it replaced:
 *  - cards keep a consistent ~`min`px width and never stretch to fill the
 *    row on wide screens (the old `auto-fit … 1fr` ballooned them),
 *  - tall cards next to short ones pack vertically instead of forcing a
 *    row-aligned "staircase" with big gaps (e.g. the System tab's backup
 *    list next to the short Export/Raw cards), and
 *  - cards fill in source order, top-to-bottom then left-to-right, height-
 *    balanced across the same column count multicol picks for
 *    `column-width: min` — so placement matches the old layout.
 *
 * Deliberately NOT CSS multi-column: multicol is a fragmentation context,
 * and WebKit / older Chromium (e.g. the Tesla in-car browser) mis-paint
 * fragmented boxes that combine `backdrop-filter`, `overflow: hidden` and
 * absolutely-positioned children — all of which PrefCard uses. Symptoms
 * were blank "ghost" cards and the disabled-state overlay floating
 * detached from its card chrome.
 *
 * Layout is measured + absolutely positioned over the flattened DOM
 * children, so a section component that renders several cards in a
 * fragment (UpdateSection, PrivacyCards) contributes each card
 * individually — exactly how multicol treated them. Positions live in
 * inline styles, not React state, so cards never remount when they move.
 */
export function PrefGrid({ children, min = 340 }: { children: ReactNode; min?: number }) {
  const ref = useRef<HTMLDivElement>(null)

  // Intentionally no dependency array: a re-render can add or remove cards,
  // so the observers re-attach to the current child nodes every render.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return

    const ro = new ResizeObserver(() => layout()) // container width + card heights (async content)
    const layout = () => {
      const kids = Array.from(el.children) as HTMLElement[]
      for (const kid of kids) ro.observe(kid) // no-op when already observed
      if (kids.length === 0) {
        el.style.height = "0px"
        return
      }
      const width = el.clientWidth
      const n = Math.max(1, Math.floor((width + GRID_GAP_PX) / (min + GRID_GAP_PX)))
      const colWidth = (width - (n - 1) * GRID_GAP_PX) / n

      // Batch the width writes before the height reads — one reflow. The
      // style-equality guards keep observer-triggered re-runs mutation-free
      // so they can't loop.
      for (const kid of kids) {
        const w = `${colWidth}px`
        if (kid.style.width !== w) kid.style.width = w
        if (kid.style.position !== "absolute") kid.style.position = "absolute"
      }
      const heights = kids.map((kid) => kid.offsetHeight)

      const runs = balancedRuns(heights, n)
      let i = 0
      let tallest = 0
      runs.forEach((count, col) => {
        const left = `${col * (colWidth + GRID_GAP_PX)}px`
        let y = 0
        for (let j = 0; j < count; j++, i++) {
          const kid = kids[i]
          if (kid.style.left !== left) kid.style.left = left
          const top = `${y}px`
          if (kid.style.top !== top) kid.style.top = top
          y += heights[i] + GRID_GAP_PX
        }
        tallest = Math.max(tallest, y - GRID_GAP_PX)
      })
      const h = `${tallest}px`
      if (el.style.height !== h) el.style.height = h
    }

    layout() // before first paint — no unpositioned flash
    ro.observe(el)
    const mo = new MutationObserver(layout) // a section mounting/unmounting a card
    mo.observe(el, { childList: true })
    return () => {
      ro.disconnect()
      mo.disconnect()
    }
  })

  return (
    <div ref={ref} className="relative">
      {children}
    </div>
  )
}
