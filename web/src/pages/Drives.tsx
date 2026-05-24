import { useCallback, useEffect, useMemo, useState } from "react"
import { Loader2 } from "lucide-react"
import { setDriveTags } from "@/api/drives"
import { DriveRow } from "@/components/drives/DriveRow"
import { DrivesActionsBar } from "@/components/drives/DrivesActionsBar"
import { DrivesToolbar } from "@/components/drives/DrivesToolbar"
import { Pagination } from "@/components/drives/Pagination"
import { useDrivesList } from "@/hooks/useDrivesList"

export default function Drives() {
  const list = useDrivesList()
  // Distance/speed unit preference, sourced from setup config
  // (DRIVE_MAP_UNIT). Default to imperial — matches the wizard default
  // and Dashboard's behaviour before the config fetch resolves, so the
  // first paint never shows a unit the user didn't pick. The fetch
  // pattern mirrors Dashboard.tsx / Viewer.tsx / FSDAnalytics.tsx.
  const [metric, setMetric] = useState(false)
  useEffect(() => {
    let cancelled = false
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        if (cancelled) return
        const entry = cfg?.DRIVE_MAP_UNIT
        if (!entry) return
        const val =
          typeof entry === "object" ? (entry.active ? entry.value : null) : entry
        if (val !== null && val !== undefined) setMetric(val === "km")
      })
      .catch(() => {
        /* non-critical — fall back to default unit */
      })
    return () => {
      cancelled = true
    }
  }, [])
  const [selectMode, setSelectMode] = useState(false)
  const [selected, setSelected] = useState<Set<number>>(new Set())

  const toggleSelectMode = () => {
    setSelectMode((s) => {
      if (s) setSelected(new Set())
      return !s
    })
  }

  const onToggleSelected = useCallback((id: number) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }, [])

  const onSelectAll = useCallback(() => {
    setSelected(new Set(list.visible.map((d) => d.id)))
  }, [list.visible])

  const onTagsChange = useCallback(
    async (id: number, tags: string[]) => {
      // Snapshot the previous tags so we can roll back if the PUT fails.
      // Optimistic update keeps the popover snappy and avoids a full
      // /api/drives refetch on every tag click. The backend invalidates
      // the drive-list cache on set_drive_tags, so the next natural fetch
      // (page revisit, manual refresh) rebuilds authoritatively.
      const prev = list.drives.find((d) => d.id === id)?.tags ?? []
      list.patchDriveTags(id, tags)
      try {
        await setDriveTags(id, tags)
      } catch (e) {
        list.patchDriveTags(id, prev)
        throw e
      }
    },
    [list],
  )

  const sortIcon = list.sortDir === "desc" ? "↓" : "↑"
  const toggleSort = () =>
    list.setSortDir(list.sortDir === "desc" ? "asc" : "desc")

  const pagination = useMemo(
    () => (
      <Pagination
        page={list.page}
        pageCount={list.pageCount}
        pageStart={list.pageStart}
        pageEnd={list.pageEnd}
        total={list.total}
        onChange={list.setPage}
      />
    ),
    [list.page, list.pageCount, list.pageStart, list.pageEnd, list.total, list.setPage],
  )

  return (
    <div className="mx-auto w-full max-w-5xl px-4 py-6 sm:px-6 sm:py-8">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3 sm:mb-6">
        <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">Drives</h1>
        <DrivesActionsBar onChanged={list.refresh} />
      </div>

      <DrivesToolbar
        drives={list.drives}
        range={list.range}
        filters={list.filters}
        onRangeChange={list.setRange}
        onFiltersChange={list.setFilters}
        selectMode={selectMode}
        onToggleSelectMode={toggleSelectMode}
        selectedCount={selected.size}
        totalCount={list.total}
        onSelectAll={onSelectAll}
        onTagSelected={() => alert("Bulk tag is not implemented yet.")}
        onExportSelected={() => alert("Bulk export is not implemented yet.")}
        onDeleteSelected={() => alert("Bulk delete is not implemented yet.")}
        metric={metric}
      />

      <div className="mt-4 flex items-center justify-between text-sm text-slate-400">
        {pagination}
        <button
          type="button"
          onClick={toggleSort}
          className="rounded-md px-2 py-1 text-slate-300 hover:bg-white/5"
        >
          Date {sortIcon}
        </button>
      </div>

      <div className="mt-3 flex flex-col gap-3">
        {list.loading && (
          <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading drives…
          </div>
        )}
        {list.error && !list.loading && (
          <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
            Failed to load drives: {list.error}
          </div>
        )}
        {!list.loading && !list.error && list.visible.length === 0 && (
          <div className="rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-center text-sm text-slate-400">
            No drives match these filters.
            <button
              type="button"
              onClick={() => {
                list.setFilters({})
                list.setRange({ kind: "preset", preset: "all" })
              }}
              className="ml-2 text-emerald-300 underline-offset-2 hover:underline"
            >
              Clear filters
            </button>
          </div>
        )}
        {!list.loading &&
          list.visible.map((d) => (
            <DriveRow
              key={d.id}
              drive={d}
              routePoints={list.routesByStartTime.get(d.startTime) ?? []}
              metric={metric}
              selectMode={selectMode}
              selected={selected.has(d.id)}
              onToggleSelected={onToggleSelected}
              onTagsChange={onTagsChange}
            />
          ))}
      </div>

      {!list.loading && list.visible.length > 0 && (
        <div className="mt-4 flex justify-end">{pagination}</div>
      )}
    </div>
  )
}
