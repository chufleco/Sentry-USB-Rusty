import { useEffect, useMemo, useRef, useState } from "react"
import { Filter } from "lucide-react"
import { cn } from "@/lib/utils"
import type { DrivesFilters } from "@/hooks/useDrivesList"
import type { DriveSummary } from "@/types/drives"

interface FilterPopoverProps {
  drives: DriveSummary[]
  filters: DrivesFilters
  onChange: (f: DrivesFilters) => void
}

export function FilterPopover({ drives, filters, onChange }: FilterPopoverProps) {
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)
  const [draft, setDraft] = useState<DrivesFilters>(filters)

  /* eslint-disable-next-line react-hooks/set-state-in-effect */
  useEffect(() => setDraft(filters), [filters])

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [open])

  const origins = useMemo(() => collectLocations(drives, "startLocation"), [drives])
  const destinations = useMemo(() => collectLocations(drives, "endLocation"), [drives])
  const tags = useMemo(() => collectTags(drives), [drives])

  const apply = () => {
    onChange(draft)
    setOpen(false)
  }
  const reset = () => {
    setDraft({})
    onChange({})
    setOpen(false)
  }

  const activeCount =
    (filters.origin ? 1 : 0) +
    (filters.destination ? 1 : 0) +
    (filters.tag ? 1 : 0) +
    (filters.minDistanceMi !== undefined ? 1 : 0)

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "inline-flex items-center gap-2 rounded-full border px-3.5 py-1.5 text-sm font-medium transition-colors",
          activeCount > 0
            ? "border-emerald-400/40 bg-emerald-400/10 text-emerald-200 hover:bg-emerald-400/20"
            : "border-white/10 bg-white/[0.03] text-slate-200 hover:bg-white/[0.06]",
        )}
      >
        <Filter className="h-4 w-4" />
        Filter
        {activeCount > 0 && (
          <span className="rounded-full bg-emerald-400/20 px-1.5 text-xs">{activeCount}</span>
        )}
      </button>
      {open && (
        <div className="absolute left-0 top-full z-50 mt-2 w-72 rounded-xl border border-white/10 bg-slate-900/95 p-3 shadow-2xl backdrop-blur">
          <Select
            label="Origin"
            value={draft.origin ?? ""}
            options={origins}
            onChange={(v) => setDraft({ ...draft, origin: v || undefined })}
          />
          <Select
            label="Destination"
            value={draft.destination ?? ""}
            options={destinations}
            onChange={(v) => setDraft({ ...draft, destination: v || undefined })}
          />
          <Select
            label="Tag"
            value={draft.tag ?? ""}
            options={tags}
            onChange={(v) => setDraft({ ...draft, tag: v || undefined })}
          />
          <NumberInput
            label="Minimum distance (mi)"
            value={draft.minDistanceMi}
            onChange={(v) => setDraft({ ...draft, minDistanceMi: v })}
          />
          <div className="mt-3 flex gap-2">
            <button
              type="button"
              onClick={reset}
              className="flex-1 rounded-md border border-white/10 bg-white/[0.03] px-2 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06]"
            >
              Reset
            </button>
            <button
              type="button"
              onClick={apply}
              className="flex-1 rounded-md bg-emerald-500/90 px-2 py-1.5 text-xs font-medium text-slate-950 hover:bg-emerald-400"
            >
              Apply filters
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

interface SelectProps {
  label: string
  value: string
  options: string[]
  onChange: (v: string) => void
}

function Select({ label, value, options, onChange }: SelectProps) {
  return (
    <label className="mb-2 block">
      <span className="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-slate-500">
        {label}
      </span>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="w-full rounded-md border border-white/10 bg-slate-950/60 px-2 py-1.5 text-sm text-slate-100 focus:border-emerald-400/40 focus:outline-none"
      >
        <option value="">Anywhere</option>
        {options.map((o) => (
          <option key={o} value={o}>
            {o}
          </option>
        ))}
      </select>
    </label>
  )
}

interface NumberInputProps {
  label: string
  value: number | undefined
  onChange: (v: number | undefined) => void
}

function NumberInput({ label, value, onChange }: NumberInputProps) {
  return (
    <label className="mb-2 block">
      <span className="mb-1 block text-[10px] font-semibold uppercase tracking-wider text-slate-500">
        {label}
      </span>
      <input
        type="number"
        min={0}
        step={0.1}
        value={value ?? ""}
        onChange={(e) => {
          const n = Number(e.target.value)
          onChange(e.target.value === "" || !Number.isFinite(n) ? undefined : n)
        }}
        className="w-full rounded-md border border-white/10 bg-slate-950/60 px-2 py-1.5 text-sm text-slate-100 focus:border-emerald-400/40 focus:outline-none"
        placeholder="0"
      />
    </label>
  )
}

function collectLocations(drives: DriveSummary[], key: "startLocation" | "endLocation"): string[] {
  const set = new Set<string>()
  for (const d of drives) {
    const v = d[key]
    if (v) set.add(v)
  }
  return Array.from(set).sort()
}

function collectTags(drives: DriveSummary[]): string[] {
  const set = new Set<string>()
  for (const d of drives) for (const t of d.tags ?? []) set.add(t)
  return Array.from(set).sort()
}
