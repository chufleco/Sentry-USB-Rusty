import { useEffect, useState } from "react"
import { ShieldCheck, Check, X, Loader2, FileText } from "lucide-react"
import { PrefCard, PrefGrid } from "@/components/settings/PrefCard"
import { cn } from "@/lib/utils"

/**
 * Settings → Privacy. Lets users review the disclosure and flip the
 * analytics opt-in at any time. This is the Art. 21 right-to-object
 * mechanism required for legitimate-interest processing — automated
 * means, no email needed.
 */
export function PrivacyTab() {
  const [choice, setChoice] = useState<boolean | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loaded, setLoaded] = useState(false)

  useEffect(() => {
    fetch("/api/config/preference?key=analytics_opt_in")
      .then((r) => r.json())
      .then((data) => {
        if (typeof data?.value === "boolean") setChoice(data.value)
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }, [])

  async function persist(value: boolean) {
    setSaving(true)
    setError(null)
    try {
      const res = await fetch("/api/config/preference", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ key: "analytics_opt_in", value }),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setChoice(value)
    } catch (e) {
      setError(`Couldn't save: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setSaving(false)
    }
  }

  return (
    <PrefGrid min={300}>
      <PrefCard
        icon={<ShieldCheck className="h-3.5 w-3.5" />}
        halo="accent"
        title="Analytics opt-in"
      >
        <p className="t-xs">
          When opted in, daily update checks include a one-way hashed device
          ID (derived from your board serial) so we can count unique installs
          without double-counting reinstalls. When opted out, nothing
          identifying is sent on update checks.
        </p>

        <div className="mt-1 flex flex-col gap-2 sm:flex-row">
          <button
            type="button"
            disabled={saving || !loaded}
            onClick={() => persist(true)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-xs font-medium transition-colors disabled:opacity-50",
              choice === true
                ? "border-emerald-400/60 bg-emerald-500/15 text-emerald-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== true ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <Check className="h-3.5 w-3.5" />
            )}
            Opted in
          </button>
          <button
            type="button"
            disabled={saving || !loaded}
            onClick={() => persist(false)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-xs font-medium transition-colors disabled:opacity-50",
              choice === false
                ? "border-slate-400/60 bg-slate-500/15 text-slate-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== false ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <X className="h-3.5 w-3.5" />
            )}
            Opted out
          </button>
        </div>

        {choice === null && loaded && !error && (
          <p className="mt-2 text-[11px] text-slate-500">
            Not set. Default is opted out — no choice means no tracking.
          </p>
        )}
        {error && (
          <p className="mt-2 text-[11px] text-rose-400">{error}</p>
        )}
      </PrefCard>

      <PrefCard
        icon={<FileText className="h-3.5 w-3.5" />}
        halo="slate"
        title="What we send, and when"
      >
        <div className="divide-y divide-white/5">
          <FlowRow
            when="Daily update check"
            what="Version, architecture, board model"
            note="Device identifier only if opted in above."
          />
          <FlowRow
            when="Once per install"
            what="Empty ping (no body, no identifier)"
            note="Anonymous gross-install counter."
          />
          <FlowRow
            when="Sentry Cloud (if signed in)"
            what="Account credentials + synced files"
            note="Stop using Cloud to stop this."
          />
          <FlowRow
            when="Wraps / lock chime submissions"
            what="The file + your IP for rate-limiting"
            note="No device fingerprint."
          />
          <FlowRow
            when="iOS push pairing"
            what="A random pairing ID"
            note="Not tied to your hardware."
          />
        </div>
        <div className="tile-divider" />
        <div className="flex flex-col gap-1">
          <a
            href="https://sentry-six.com/privacy"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            Full privacy policy ↗
          </a>
          <a
            href="https://github.com/Sentry-Six/Sentry-USB-Rusty/wiki/Privacy"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            Wiki: what each flow does ↗
          </a>
        </div>
      </PrefCard>
    </PrefGrid>
  )
}

function FlowRow({
  when,
  what,
  note,
}: {
  when: string
  what: string
  note?: string
}) {
  return (
    <div className="py-2">
      <p className="text-xs font-semibold text-slate-300">{when}</p>
      <p className="mt-0.5 text-[11px] text-slate-400">{what}</p>
      {note && (
        <p className="mt-0.5 text-[11px] italic text-slate-500/80">{note}</p>
      )}
    </div>
  )
}
