import { useEffect, useRef, useState } from "react"
import { Settings2 } from "lucide-react"
import { useChargingRates } from "@/hooks/useChargingRates"

// Opens an editor for the electricity rates used to cost charge
// sessions: a currency symbol, a default price-per-kWh, and an optional
// per-tag override for each known charging tag. Saving persists the
// prefs and calls `onSaved` so the page can refetch sessions (cost is
// computed server-side from these values).
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

  const onSave = async () => {
    setBusy(true)
    try {
      const parsedDefault = parseRate(defaultRate)
      const cleanTagRates: Record<string, number> = {}
      for (const [k, v] of Object.entries(tagRates)) {
        const n = parseRate(v)
        if (n != null) cleanTagRates[k] = n
      }
      await save({
        currency: currency.trim() || "$",
        defaultRate: parsedDefault,
        tagRates: cleanTagRates,
      })
      setOpen(false)
      onSaved?.()
    } finally {
      setBusy(false)
    }
  }

  const tagEntries = Object.keys(tagRates).sort((a, b) => a.localeCompare(b))

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
        <div className="absolute right-0 top-full z-50 mt-2 w-72 rounded-xl border border-white/10 bg-slate-900/95 p-3 shadow-2xl backdrop-blur">
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
