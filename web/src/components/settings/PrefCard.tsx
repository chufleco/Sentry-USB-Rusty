import type { ReactNode } from "react"
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

/**
 * Card wrapper used by every settings tab. Masonry via CSS multi-column:
 *  - cards keep a consistent ~`min`px width and never stretch to fill the
 *    row on wide screens (the old `auto-fit … 1fr` ballooned them), and
 *  - tall cards next to short ones pack vertically instead of forcing a
 *    row-aligned "staircase" with big gaps (e.g. the System tab's backup
 *    list next to the short Export/Raw cards).
 * Each card is kept whole with `break-inside-avoid`.
 */
export function PrefGrid({ children, min = 340 }: { children: ReactNode; min?: number }) {
  return (
    <div
      className="[&>*]:mb-2.5 [&>*]:break-inside-avoid"
      style={{ columnWidth: `${min}px`, columnGap: "0.625rem" }}
    >
      {children}
    </div>
  )
}
