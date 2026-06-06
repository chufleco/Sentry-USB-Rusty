import {
  ChargingSection,
  type ChargeDetail,
} from "@/components/drives/ChargingSection"

const MOCK: ChargeDetail = {
  powerKw: 7,
  currentA: 16,
  voltageV: 240,
  rateMph: 25,
  energyAddedKwh: 12.4,
  limitSoc: 80,
  rangeMi: 263,
}

export default function PreviewCharging() {
  return (
    <div className="min-h-screen bg-slate-950 text-slate-100">
      <div className="mx-auto max-w-3xl space-y-6 p-6">
        <div>
          <h1 className="text-2xl font-bold">Charging — preview</h1>
          <p className="mt-1 text-sm text-slate-500">
            Dev-only preview of the Charging section for the Driving view.
            Values shown are mock data.
          </p>
        </div>
        <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
          <ChargingSection charge={MOCK} />
        </div>
      </div>
    </div>
  )
}
