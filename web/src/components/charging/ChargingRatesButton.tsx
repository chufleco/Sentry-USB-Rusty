import { useEffect, useRef, useState } from "react"
import { Plus, Settings2, X } from "lucide-react"
import { useChargingRates, type TouPeriod } from "@/hooks/useChargingRates"

interface PeriodDraft {
  label: string
  start: string
  end: string
  rate: string
}

const TIME_RE = /^\d{1,2}:\d{2}$/

// Opens an editor for the electricity rates used to cost charge
// sessions: a currency symbol, a default price-per-kWh, an optional
// per-tag override, and an optional time-of-use schedule. Saving
// persists the prefs and calls `onSaved` so the page can refetch
// sessions (cost is computed server-side from these values).
export function ChargingRatesButton({
  tags,
  onSaved,
}: {
  tags: string[]
  onSaved?: () => void
}) {
  const { rates, loading, save } = useChargingRates()
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)

  const [currency, setCurrency] = useState("$")
  const [defaultRate, setDefaultRate] = useState("")
  const [tagRates, setTagRates] = useState<Record<string, string>>({})
  const [touEnabled, setTouEnabled] = useState(false)
  const [touDraft, setTouDraft] = useState<PeriodDraft[]>([])
  const [busy, setBusy] = useState(false)

  // Seed the draft from the loaded rates + known tags, then open. The
  // Rates button stays disabled until rates finish loading, so this
  // always runs with the saved values in hand.
  const openEditor = () => {
    setCurrency(rates.currency)
    setDefaultRate(rates.defaultRate != null ? String(rates.defaultRate) : "")
    const draft: Record<string, string> = {}
    for (const t of tags) {
      draft[t] = rates.tagRates[t] != null ? String(rates.tagRates[t]) : ""
    }
    // Keep rates for tags that exist in prefs but aren't in the current
    // list (e.g. a renamed tag) so saving doesn't drop them.
    for (const [k, v] of Object.entries(rates.tagRates)) {
      if (!(k in draft)) draft[k] = String(v)
    }
    setTagRates(draft)
    setTouEnabled(rates.touEnabled)
    setTouDraft(
      rates.touPeriods.map((p) => ({
        label: p.label,
        start: p.start,
        end: p.end,
        rate: String(p.rate),
      })),
    )
    setOpen(true)
  }

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [open])

  const updatePeriod = (i: number, field: keyof PeriodDraft, value: string) =>
    setTouDraft((d) => d.map((p, idx) => (idx === i ? { ...p, [field]: value } : p)))
  const addPeriod = () =>
    setTouDraft((d) => [
      ...d,
      { label: "", start: "22:00", end: "06:00", rate: "" },
    ])
  const removePeriod = (i: number) =>
    setTouDraft((d) => d.filter((_, idx) => idx !== i))

  const onSave = async () => {
    setBusy(true)
    try {
      const cleanTagRates: Record<string, number> = {}
      for (const [k, v] of Object.entries(tagRates)) {
        const n = parseRate(v)
        if (n != null) cleanTagRates[k] = n
      }
      const touPeriods: TouPeriod[] = []
      for (const p of touDraft) {
        const rate = parseRate(p.rate)
        if (rate == null || !TIME_RE.test(p.start) || !TIME_RE.test(p.end)) {
          continue
        }
        touPeriods.push({ label: p.label.trim(), start: p.start, end: p.end, rate })
      }
      await save({
        currency: currency.trim() || "$",
        defaultRate: parseRate(defaultRate),
        tagRates: cleanTagRates,
        touEnabled,
        touPeriods,
      })
      setOpen(false)
      onSaved?.()
    } finally {
      setBusy(false)
    }
  }

  const tagEntries = Object.keys(tagRates).sort((a, b) => a.localeCompare(b))
  const timeClass =
    "rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 [color-scheme:dark] focus:border-emerald-400/40 focus:outline-none"

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        disabled={loading}
        onClick={() => (open ? setOpen(false) : openEditor())}
        className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-2.5 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/[0.06] disabled:opacity-50"
      >
        <Settings2 className="h-3.5 w-3.5" />
        Rates
      </button>
      {open && (
        <div className="absolute right-0 top-full z-50 mt-2 max-h-[80vh] w-80 overflow-y-auto rounded-xl border border-white/10 bg-slate-900/95 p-3 shadow-2xl backdrop-blur">
          <div className="mb-2 text-xs font-semibold uppercase tracking-wider text-slate-400">
            Electricity rates
          </div>

          <div className="mb-3 flex gap-2">
            <label className="flex w-16 flex-col gap-1">
              <span className="text-[10px] uppercase tracking-wide text-slate-500">
                Symbol
              </span>
              <input
                type="text"
                value={currency}
                maxLength={3}
                onChange={(e) => setCurrency(e.target.value)}
                className="w-full rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 focus:border-emerald-400/40 focus:outline-none"
              />
            </label>
            <label className="flex flex-1 flex-col gap-1">
              <span className="text-[10px] uppercase tracking-wide text-slate-500">
                Default rate / kWh
              </span>
              <input
                type="number"
                inputMode="decimal"
                step="0.01"
                min="0"
                placeholder="e.g. 0.30"
                value={defaultRate}
                onChange={(e) => setDefaultRate(e.target.value)}
                className="w-full rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
              />
            </label>
          </div>

          <div className="mb-1 text-[10px] uppercase tracking-wide text-slate-500">
            Per-tag rate / kWh
          </div>
          {tagEntries.length === 0 ? (
            <p className="mb-3 rounded-md bg-white/[0.02] px-2 py-1.5 text-xs text-slate-500">
              Tag a charge to set a rate for it. Tagged charges use the
              highest matching tag rate; the rest use the default.
            </p>
          ) : (
            <div className="mb-3 flex max-h-40 flex-col gap-1.5 overflow-y-auto pr-1">
              {tagEntries.map((t) => (
                <label key={t} className="flex items-center gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm text-slate-200">
                    {t}
                  </span>
                  <input
                    type="number"
                    inputMode="decimal"
                    step="0.01"
                    min="0"
                    placeholder="default"
                    value={tagRates[t]}
                    onChange={(e) =>
                      setTagRates((prev) => ({ ...prev, [t]: e.target.value }))
                    }
                    className="w-24 rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
                  />
                </label>
              ))}
            </div>
          )}

          <div className="mb-3 border-t border-white/[0.06] pt-3">
            <label className="flex cursor-pointer items-center gap-2">
              <input
                type="checkbox"
                checked={touEnabled}
                onChange={(e) => setTouEnabled(e.target.checked)}
                className="h-4 w-4 accent-emerald-500"
              />
              <span className="text-[10px] font-semibold uppercase tracking-wide text-slate-400">
                Time-of-use pricing
              </span>
            </label>

            {touEnabled && (
              <div className="mt-2 flex flex-col gap-2">
                {touDraft.length === 0 && (
                  <p className="rounded-md bg-white/[0.02] px-2 py-1.5 text-xs text-slate-500">
                    Add periods like off-peak 22:00–06:00. Hours not covered
                    use the default rate; tagged charges keep their tag rate.
                  </p>
                )}
                {touDraft.map((p, i) => (
                  <div
                    key={i}
                    className="rounded-md border border-white/10 bg-white/[0.02] p-2"
                  >
                    <div className="flex items-center gap-1.5">
                      <input
                        type="text"
                        value={p.label}
                        onChange={(e) => updatePeriod(i, "label", e.target.value)}
                        placeholder="Label (e.g. Off-peak)"
                        className="min-w-0 flex-1 rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
                      />
                      <button
                        type="button"
                        aria-label="Remove period"
                        onClick={() => removePeriod(i)}
                        className="shrink-0 rounded-md p-1 text-slate-500 hover:bg-white/5 hover:text-slate-300"
                      >
                        <X className="h-3.5 w-3.5" />
                      </button>
                    </div>
                    <div className="mt-1.5 flex items-center gap-1.5">
                      <input
                        type="time"
                        value={p.start}
                        onChange={(e) => updatePeriod(i, "start", e.target.value)}
                        className={timeClass}
                      />
                      <span className="text-xs text-slate-500">to</span>
                      <input
                        type="time"
                        value={p.end}
                        onChange={(e) => updatePeriod(i, "end", e.target.value)}
                        className={timeClass}
                      />
                      <input
                        type="number"
                        inputMode="decimal"
                        step="0.01"
                        min="0"
                        placeholder="/ kWh"
                        value={p.rate}
                        onChange={(e) => updatePeriod(i, "rate", e.target.value)}
                        className="w-20 rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
                      />
                    </div>
                  </div>
                ))}
                <button
                  type="button"
                  onClick={addPeriod}
                  className="inline-flex items-center justify-center gap-1 rounded-md border border-white/10 bg-white/[0.03] px-2 py-1 text-xs font-medium text-slate-300 transition-colors hover:bg-white/[0.06]"
                >
                  <Plus className="h-3.5 w-3.5" />
                  Add period
                </button>
              </div>
            )}
          </div>

          <button
            type="button"
            disabled={busy}
            onClick={onSave}
            className="w-full rounded-md bg-emerald-500/90 px-2.5 py-1.5 text-xs font-medium text-slate-950 transition-colors hover:bg-emerald-400 disabled:opacity-50"
          >
            Save rates
          </button>
        </div>
      )}
    </div>
  )
}

// Parse a rate input into a finite, non-negative number, or null for
// blank/invalid (so the field clears the stored rate).
function parseRate(s: string): number | null {
  const t = s.trim()
  if (t === "") return null
  const n = Number(t)
  return Number.isFinite(n) && n >= 0 ? n : null
}
