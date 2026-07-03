# POS_MIGRATION_RUNBOOK — cutting the Flutter fleet over to the unified menu model

How the deployed Flutter POS moves from the legacy menu/recipe/modifier model to the unified one with
**zero downtime** and **no forced client update**, using the compat shim. Grounded in the frozen
`CONTRACT.md`. Money is integer piastres; order history is immutable; catalog sync is revision-gated.

## Principle: strangler-fig, shim-backed

Old Flutter builds (old `madar-core` + `madar-api`) keep hitting the **old endpoints/JSON shapes**, which
after the flip are served from the compat **views** + a stable-id order-create translation. New Flutter
builds hit the **new endpoints** and sync via `catalog_revision`. Both run concurrently against one
backend until the fleet is fully updated; then the shim is torn down (`SHIM_TEARDOWN_PROMPT.md`).

## Rollout sequence — REHEARSED END-TO-END 2026-07-03 on `madar_fresh` (all checks passed)

**Locked decisions:** hard **DROP** of legacy tables (the pg_dump is the rollback); the dashboard
deploys **with** the backend (Menu Studio + `menu-price-overrides` are the only catalog-write surfaces —
every legacy dashboard write flow was rewired off the legacy endpoints); legacy catalog WRITES after the
flip fail fast as **409** (SQLSTATE 55000 "cannot insert into view …" mapped in `errors.rs`) — nothing
legitimate calls them (deployed POS only writes orders).

**Stage 0 — pre-flight (no user impact).**
- `pg_dump` prod (`db-backups/`) — this IS the rollback. Confirm the `sufrix` role exists.
- Deploy the new backend: boot auto-applies the additive EXPAND migration
  `20260703100000_menu_unification_expand.sql`; legacy tables stay live; nothing reads the new tables
  yet. All old endpoints unchanged. (Demo seeding/sweeper already target the new tables; `catalog/sync`
  accepts an omitted `channel` for branch-only POS resolution.)

**Stage 1 — backfill (no user impact).**
- Per org: `backfill-menu-unification --org <uuid> --dry-run`, review the report (expect only
  `info.implicit_all_addons`), then run live. Idempotent; safe to re-run. Order history untouched.
- Rehearsal result: 4 orgs, 0 unmigratable failures (2 informational notes).
- Run `bundle-margin-flip`; re-price/complete recipes for any flagged bundle (rehearsal: 0 flips).

**Stage 2 — the flip (one command + dashboard deploy).**
- Optionally re-run the backfill per org to capture writes made since Stage 1 (idempotent).
- Apply `deploy/menu_unification_shim.sql` (single transaction: DROPs the 12 legacy catalog/override
  tables + the dead `menu_item_addon_overrides`, recreates the 12 as read-only views). **This is the
  moment the source of truth flips.** Expected NOTICEs: the three order-history FK constraints drop
  (stable ids keep resolving — verified).
- Deploy the dashboard (auto-deploy) — same release window.
- Post-flip verification (rehearsed, all green):
  - legacy reads serve full data through the views (addons/recipes/allowlist/sizes + costing joins);
  - a legacy catalog write raises 55000 → API returns 409, not 500;
  - order-create pathway intact: catalog resolves by stable id via views; `order_item_addons` INSERT ok;
  - new tables populated (groups/options/attaches/recipe_lines/overrides/catalog_revision).
- **Deployed old Flutter clients are unaffected** — catalog reads come from the views byte-for-byte
  (validated: `CONTRACT.md §8`), order posts pass through stable ids.

**Stage 3 — ship the new Flutter build (gradual).**
- Roll out the new build (updated `madar-frb` + `rust_bridge` + `app_core` + `apps/madar`) to a canary
  branch first. It syncs via `catalog_revision`: on boot / periodically it calls `GET /catalog/sync?since=…`;
  when the server revision is higher it pulls the unified catalog and prices/availability resolved for its
  `(branch, channel)`.
- Verify offline pricing parity: the new client's offline cart total must equal the old client's for the
  same order (the rust-core parity test from Wave 3 gates this).
- Widen the rollout as canary holds. Old and new builds coexist safely.

**Stage 4 — teardown (after 100% adoption).**
- When telemetry shows no device on the old build and no legacy-endpoint hits for N days, execute
  `SHIM_TEARDOWN_PROMPT.md` (drop views + legacy endpoints + dead columns + the retired cost engine/diff bin).

## Catalog resync mechanics
- `catalog_revision(org_id, revision)` bumps on every catalog write (Wave-2). The POS caches its last synced
  revision; a higher server revision triggers a pull. Price/availability are pre-resolved per `(branch,
  channel)` per `CONTRACT.md §3` — the client does not re-derive overrides.

## Rollback
- **Decision: hard DROP** (no `*_legacy` renames). The Stage-0 `pg_dump` is the rollback: restore the
  dump + redeploy the previous backend image. Order history is never part of any rollback (immutable,
  self-resolving); catalog edits made between flip and rollback would be lost — keep that window short.
- **Before Stage 2 flip:** trivial — nothing reads new tables; drop them if desired (data loss is only the
  backfill, re-runnable). The EXPAND migration is additive and harmless to leave in place.
- **After the flip:** the shim views + old backend image are the rollback path — redeploy the previous
  backend image and `DROP VIEW … ; ALTER TABLE … RENAME` the legacy tables back **only if** the legacy
  tables were renamed rather than dropped. RECOMMENDATION for the flip release: **rename** legacy tables to
  `*_legacy` instead of `DROP` (adjust `deploy/menu_unification_shim.sql` to `ALTER TABLE … RENAME TO
  *_legacy` before creating the views), keep them for one release cycle as a hot rollback, and drop them at
  teardown. Order history is never part of any rollback — it is immutable and self-resolving.

## Monitoring during cutover
- New vs old build mix (by `catalog_revision`-aware sync calls vs legacy catalog GETs).
- 5xx on new endpoints; any legacy order-create translation failures.
- Bundle margin-floor: run `bundle-margin-flip` before the flip and re-price/complete recipes for every
  flagged bundle (`PASS→FAIL` / `*→UNKNOWN`) so activation stays valid under the new engine.
