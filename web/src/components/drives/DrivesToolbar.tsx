import { CheckSquare } from "lucide-react"
import type { DateRange, DrivesFilters } from "@/hooks/useDrivesList"
import type { DriveSummary } from "@/types/drives"
import { DatePopover } from "./DatePopover"
import { FilterPopover } from "./FilterPopover"
import { SelectModeBar } from "./SelectModeBar"

interface DrivesToolbarProps {
  drives: DriveSummary[]
  range: DateRange
  filters: DrivesFilters
  onRangeChange: (r: DateRange) => void
  onFiltersChange: (f: DrivesFilters) => void
  selectMode: boolean
  onToggleSelectMode: () => void
  selectedCount: number
  totalCount: number
  onSelectAll: () => void
  onTagSelected: () => void
  onExportSelected: () => void
  onDeleteSelected: () => void
  // DRIVE_MAP_UNIT === "km" → render the FilterPopover's distance field
  // in kilometres, matching the unit shown on each drive's row.
  metric: boolean
}

export function DrivesToolbar(props: DrivesToolbarProps) {
  return (
    <div className="flex flex-wrap items-center gap-2">
      <DatePopover range={props.range} onChange={props.onRangeChange} />
      <FilterPopover
        drives={props.drives}
        filters={props.filters}
        onChange={props.onFiltersChange}
        metric={props.metric}
      />
      <div className="ml-auto flex flex-wrap items-center gap-2">
        {props.selectMode ? (
          <SelectModeBar
            selectedCount={props.selectedCount}
            totalCount={props.totalCount}
            onSelectAll={props.onSelectAll}
            onTag={props.onTagSelected}
            onExport={props.onExportSelected}
            onDelete={props.onDeleteSelected}
            onCancel={props.onToggleSelectMode}
          />
        ) : (
          <button
            type="button"
            onClick={props.onToggleSelectMode}
            className="inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.03] px-3.5 py-1.5 text-sm font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
          >
            <CheckSquare className="h-4 w-4" />
            Select
          </button>
        )}
      </div>
    </div>
  )
}
