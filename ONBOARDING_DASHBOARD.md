# Sufrix Dashboard — Organization Onboarding Experience

**Audience:** the agent working on the Sufrix React dashboard (`React 19, TypeScript, FSD, TanStack Query, shadcn/ui, react-router, OpenAPI-generated client`).
**Backend version:** includes the onboarding endpoints (regenerate the client from the new `openapi.json` first — operations live under the `orgs` tag).
**Goal:** a new organization signs in for the first time and is walked through setting up everything needed to take their first order — then never sees the wizard again.

---

## 0. Backend contract (already live)

### `GET /orgs/{id}/onboarding` → `OnboardingStatus`

```ts
{
  org_id: string,
  completed: boolean,            // persisted flag — THE routing signal
  completed_at: string | null,
  can_complete: boolean,         // all required steps done → enables Finish
  recipe_coverage: number,       // 0..1, active items that have ≥1 recipe row
  steps: OnboardingStep[],
}

// OnboardingStep
{
  key: "branch" | "payment_methods" | "categories" | "menu_items"
     | "ingredients" | "recipes" | "addons" | "team" | "first_order",
  done: boolean,
  count: number,                 // e.g. 3 menu items created
  required: boolean,             // branch/payment_methods/categories/menu_items are required
}
```

**Critical design fact:** step progress is **derived server-side from data presence** on every read. There is no client-side "mark step done" call and there must never be one — creating a branch *is* completing the branch step. The only mutation is:

### `POST /orgs/{id}/onboarding/complete` → `OnboardingStatus`

Idempotent. Sets `completed = true` permanently. Call it only from the Finish action (or Skip — see §4). Orgs that already had orders were backfilled to `completed = true`, so existing customers never see the wizard.

Permissions: GET needs `orgs:read`, complete needs `orgs:update` — i.e. org admins. Don't render any onboarding UI for non-admin roles.

---

## 1. Routing & gating

1. After auth resolves the active org scope, fetch onboarding status once: `useQuery(['onboarding', orgId], …, { staleTime: 30_000 })`.
2. If `completed === false` and the user is an org admin → redirect to `/onboarding` (a full-screen route OUTSIDE the normal app shell — no sidebar, no scope switcher; the wizard is the shell).
3. Never hard-trap the user: a quiet "Skip setup, take me to the dashboard →" link lives in the wizard footer at all times (see §4 for what it does).
4. If `completed === true`, the `/onboarding` route redirects to the overview. The wizard is unreachable after completion — re-runs are not a feature.
5. Invalidate `['onboarding', orgId]` after every create mutation fired from inside the wizard (branch created, item created, …) so the stepper reflects reality immediately.

---

## 2. Wizard structure

A single route `/onboarding` with an internal stepper (URL-synced via `?step=` so refresh/back work — match the dashboard's existing URL-state conventions). Order:

| # | Step key | Title (EN) | Required | Reuses |
|---|----------|------------|----------|--------|
| 0 | — | Welcome | — | new |
| 1 | `branch` | Create your first branch | ✅ | existing branch form |
| 2 | `payment_methods` | How do customers pay? | ✅ | existing payment-methods manager |
| 3 | `categories` + `menu_items` | Build your menu | ✅ | existing category + item forms |
| 4 | `ingredients` + `recipes` | Ingredient costs (optional, recommended) | ⬜ | existing inventory + recipe editor |
| 5 | `addons` | Addons & extras (optional) | ⬜ | existing addon manager |
| 6 | `team` | Invite your team (optional) | ⬜ | existing user form |
| 7 | — | Review & finish | — | new |

**Reuse, don't rebuild.** Every step embeds the *existing* feature's form/list components in a narrowed layout. If a form is currently page-coupled, extract the form into its feature's segment so both the page and the wizard import it — do not fork the markup. The wizard contributes: framing copy, the stepper, progress logic, and empty-state illustrations. Nothing else is new CRUD.

### Step behaviors

- **Welcome (0):** org name + logo (reuse org settings form fields), one paragraph on what's ahead, "Let's go". If the org already has partial data (admin left mid-way), say "Picking up where you left off" and jump to the first incomplete required step.
- **Branch (1):** single branch form. After create, show the success state with the branch card and "Add another / Continue".
- **Payments (2):** the payment-methods list pre-seeded by the backend (cash etc.) — the step is about toggling/adding, so `done` is usually already true; frame it as confirmation: "These are on. Add card / wallet methods if you take them."
- **Menu (3):** two-pane: categories left, items right (same components as the menu screen). Gate "Continue" on ≥1 category AND ≥1 active item (mirror the server's logic; the authoritative check is the refetched `steps`). Offer a "common café starter" hint but no fake data injection.
- **Costs (4):** this is the cost-engine on-ramp — sell it: "Add ingredient costs and Sufrix computes profit per item, classifies your menu, and suggests prices." Show `recipe_coverage` as a live progress ring. Skippable with one click; never guilt-block.
- **Addons (5) / Team (6):** thin wrappers over existing managers, both skippable.
- **Review (7):** render the `steps` array as a checklist (done = green check, optional-undone = gray "later"), the recipe-coverage ring, and the **Finish** button bound to `can_complete`. On click → `POST …/complete` → confetti-light success → route to overview. If `can_complete` is false, the button is disabled with "Finish requires: <missing required steps>" and each missing item deep-links back to its step.

---

## 3. Post-wizard: the setup checklist widget

After completion the wizard dies, but optional steps may remain undone. On the **overview screen**, render a dismissible "Setup checklist" card while any optional step has `done === false`:

- Rows from the same `steps` payload (optional ones only) + recipe coverage if `< 0.8` — each row deep-links to its screen.
- The `first_order` step turns this card into the bridge to the POS: "Open a shift on the iPad and ring up your first order."
- Dismiss state is client-side only (`localStorage` is unavailable in artifacts but fine in the real dashboard — follow the dashboard's existing persistence util). When every optional step is done, the card disappears on its own.

This widget is also where the cost-engine adoption loop closes: low `recipe_coverage` here + the "items missing costs" card on the Menu Engineering screen (see `DASHBOARD_INTEGRATION.md` §1) chase the same goal from two sides.

---

## 4. Skip semantics

"Skip setup" from any step = `POST …/complete` only if `can_complete` is true; otherwise route to the overview **without** completing — the redirect-to-wizard rule (§1.2) then applies on next sign-in, but soften it: if the user skipped this session (session-scoped flag), don't bounce them back until the next session. Never call `complete` on an org that can't take an order; a "completed" org with no branch is a lie the rest of the app will trip over.

---

## 5. FSD placement

```
features/onboarding/
  api/        → useOnboardingStatus(orgId), useCompleteOnboarding()
  model/      → step config (keys, order, required, route per step), derive-first-incomplete
  ui/         → WizardShell, Stepper, StepFrame, ReviewChecklist, CoverageRing
widgets/setup-checklist/   → the overview card (consumes features/onboarding api)
app/routing                → the completed-flag guard
```

Step config must key off the backend `key` strings verbatim — they are the contract; titles/descriptions come from i18n, never from the server.

## 6. i18n / RTL

Full Egyptian-Arabic coverage like the rest of the dashboard; the wizard is most orgs' first impression. Suggested step titles (defer to the existing glossary): الفرع، طرق الدفع، المنيو، تكلفة المكونات، الإضافات، الفريق، المراجعة والإنهاء. Stepper direction flips under RTL; verify the progress connector lines mirror correctly with the existing direction provider.

## 7. Pitfalls

- Do not store step completion anywhere client-side — the server derives it. The only client state is the current step index and the session "skipped" flag.
- `done` can flip back to `false` (admin deletes their only branch later) — the setup-checklist widget must tolerate regression; the persisted `completed` flag never regresses.
- Don't gate the wizard on `steps` order — gate on `required`/`done`; the array order may change.
- `payment_methods` counts **active** methods; a user deactivating everything in step 2 un-does the step — surface that inline.
- Test the backfill path: an org with historical orders must land on the overview, never the wizard.
