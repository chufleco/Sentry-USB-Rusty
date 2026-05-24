import { Link, useParams } from "react-router-dom"
import { ArrowLeft } from "lucide-react"

export default function DriveDetail() {
  const { id } = useParams<{ id: string }>()
  return (
    <div className="mx-auto max-w-3xl px-6 py-8 text-slate-200">
      <Link
        to="/drives"
        className="mb-6 inline-flex items-center gap-2 text-sm text-slate-400 hover:text-slate-200"
      >
        <ArrowLeft className="h-4 w-4" />
        Back to drives
      </Link>
      <h1 className="text-2xl font-semibold text-slate-100">Drive #{id}</h1>
      <p className="mt-4 text-sm text-slate-500">
        Detail view scaffolding — full layout coming in Phase 4 (map, scrubber,
        FSD stripe, dual-pin echo, stat tiles, speed chart, and conditional
        sections per the design spec).
      </p>
    </div>
  )
}
