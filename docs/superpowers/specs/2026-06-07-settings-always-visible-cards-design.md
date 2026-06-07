# Settings — always-visible cards with disabled overlay

**Status:** Draft for approval
**Author:** Claude (under Chad's direction)
**Date:** 2026-06-07
**Scope:** `/settings` page only

## Problem

A handful of `PrefCard`s on the Settings page render `null` (or are wrapped in a
conditional) when their underlying feature is off, hardware is absent, or
runtime state hasn't activated. When that happens, the surrounding CSS Grid
(`PrefGrid`) reflows: columns rebalance, sibling cards jump positions, and the
page feels unstable. Users have to remember which cards exist at all, because
some only appear after wizard steps that are easy to forget.

The user's verbatim complaint: *"we'd never need to worry about the stupid ass
sizing"*. The fix is to stop cards from disappearing.

## Goal

Every `PrefCard` on Settings always renders. When a card's feature is
unavailable, the card body is blurred and a centered overlay explains what the
user needs to enable to make it work. The card's header (icon, title, badge)
stays fully readable so the user can still tell what each card is.

Out of scope: other pages, uniform card heights, refactoring cards that are
already always-visible-with-internal-state.

## Design

### 1. `PrefCard` learns a `disabled` prop

```tsx
interface PrefCardProps {
  // existing: icon, halo, title, badge, children, etc.

  /**
   * When set, the card body is rendered behind a blurred non-interactive
   * overlay with the supplied reason text and (optionally) a CTA button.
   * The header stays fully visible so users can still identify the card.
   * Children stay mounted — re-enabling doesn't lose draft state.
   */
  disabled?: {
    reason: string
    cta?: {
      label: string
      onClick?: () => void
      href?: string  // for any future use; not exercised in this change
    }
  }
}
```

When `disabled` is set, the card renders:

- Root `<div>` gets `aria-disabled="true"` and `data-disabled="true"` for
  styling hooks and assistive tech.
- The header (icon + title + badge) renders normally on top.
- The body is rendered inside a `<div class="relative">` so the overlay can
  absolutely-position over it.
- The body's children wrapper gets:
  `blur-[2px] opacity-40 pointer-events-none select-none transition-all`.
- The overlay sits absolutely positioned over the body:
  `absolute inset-0 flex flex-col items-center justify-center gap-2 p-4 text-center`.
- Reason text: `text-xs text-slate-300 max-w-[28ch]`.
- CTA button (when present): matches the existing `bg-blue-500/15
  text-blue-400 hover:bg-blue-500/25 rounded-lg px-3 py-1.5 text-xs
  font-medium` pattern used elsewhere in Settings.

All styling lives in Tailwind utility classes inside `PrefCard.tsx`. No new
rules in `web/src/index.css`.

### 2. Three call-site updates

**a) `web/src/components/settings/sections/KeepAccessorySection.tsx`**

Today (line 22): `if (loaded && !values.enabled && !everOn.current) return null`.

After: remove the `return null`. When `loaded && !values.enabled && !everOn.current`,
pass `disabled` to `PrefCard`:

```tsx
disabled={{
  reason: "Enable 'Keep Accessory' in the Setup Wizard. This feature is only useful for 12V-powered Pis.",
  cta: { label: "Open Setup Wizard", onClick: onOpenWizard },
}}
```

`KeepAccessorySection` needs a new optional prop `onOpenWizard?: () => void`,
plumbed through from `Settings.tsx` (it already owns the `handleOpenWizard`
handler that `SystemTab` uses). When `onOpenWizard` is not provided, the CTA
is omitted — graceful degradation.

**b) `web/src/pages/settings/NetworkTab.tsx`**

Today (lines 89-115): the Away Mode AP `<PrefCard>` is wrapped in
`{awayStatus.state === "active" && (...)}`.

After: drop the conditional wrapper. Render the `PrefCard` unconditionally.
When `awayStatus.state !== "active"`:

```tsx
disabled={{
  reason: "Away Mode is off — enable it above to see the AP details.",
}}
```

No CTA — the `AwayModeControl` card sits directly above and is the toggle.

**c) `AdapterPicker` (inside `BlePairButton.tsx`)**

Unchanged. It's a sub-element of an already-always-visible `PrefCard`, not a
top-level card on the grid. Out of scope.

**d) `BlePairButton` itself**

Unchanged. It already renders always-visible with a "Disabled" pill and a
disabled action button when BLE is off, and keeps the VIN input editable so
users can prep the VIN before flipping BLE on. Retrofitting the overlay here
would hide the VIN input behind blur — a regression.

### 3. Visual interaction notes

- Re-enabling a card: when the gating condition flips from disabled → enabled,
  Tailwind's `transition-all` on the body wrapper produces a smooth blur fade
  rather than a jarring snap.
- Focus: with `pointer-events: none` on the children wrapper, Tab-focus skips
  the controls underneath. The overlay's CTA button (if present) is the only
  focusable element in the body area.
- Screen readers: `aria-disabled="true"` on the root signals the card is
  inactive. The reason text inside the overlay is a regular text node and
  reads naturally.

### 4. Reason copy — the three concrete strings

| Card | Reason text | CTA |
|---|---|---|
| Keep Accessory (wizard-gated) | "Enable 'Keep Accessory' in the Setup Wizard. This feature is only useful for 12V-powered Pis." | "Open Setup Wizard" |
| Away Mode AP (runtime-gated) | "Away Mode is off — enable it above to see the AP details." | — |

If we discover other vanishing cards during implementation, they'll be added
to this table. No silent additions — the spec stays authoritative.

## Testing

Manual verification on `npm run dev`:

1. Navigate to Settings → Device. Confirm Keep Accessory card is **visible**
   for a fresh install (where the wizard's 12V step hasn't been run).
2. Card header readable; body blurred; centered reason text + "Open Setup
   Wizard" CTA visible.
3. Click the CTA — Setup Wizard opens.
4. Complete the 12V step and close the wizard — the card body un-blurs and
   the full Keep Accessory UI is interactive.
5. Navigate to Settings → Car & Network. With Away Mode off, the Away Mode AP
   card is **visible** below `AwayModeControl`, header readable, body blurred,
   reason text shown.
6. Toggle Away Mode on. Once `awayStatus.state === "active"`, the card body
   un-blurs and shows the SSID/IP rows.
7. Layout stability: with Keep Accessory in either state and Away Mode in
   either state, the grid columns and surrounding card positions are
   identical. Nothing reflows.

No automated tests added — these are CSS/JSX presentational changes; the
existing tab-rendering tests cover the structural side.

## Risk

Low. The change is additive (new optional prop) with three small call-site
updates. The only behavior change for already-working cards is `null` →
"render with disabled overlay", which the user has explicitly requested.

If `disabled` is set and the component happens to be inside a flex/grid
parent that was relying on the card collapsing to zero size, layout might
look different there. Mitigation: only the three identified cards change
behavior; all other cards continue to render exactly as before.

## Decision log

- **Q:** Should we also enforce uniform card min-heights for maximally stable
  layout? **A:** No — out of scope (Approach C in brainstorming). If stability
  still feels off after this lands, easy follow-up.
- **Q:** Should `BlePairButton` get the overlay treatment for consistency?
  **A:** No — it's already always-visible, and overlaying it would hide the
  VIN input (regression). User confirmed.
- **Q:** Should we apply this to dialogs (SetupWizard, RawConfigEditor, etc.)?
  **A:** No — those are actual modal dialogs that only render while open;
  they don't have the "vanishing card in a grid" problem.
