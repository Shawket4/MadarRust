-- Archive inventory ingredients (owner-approved 2026-10-18).
--
-- An archived ingredient leaves the pickers, the stock-count sheets (once its
-- shelves are empty), reorder suggestions and the menu linter, and stays
-- everywhere history needs it. Archiving is reversible and changes NOTHING
-- else about the row:
--
--   * `is_active` is untouched. It is what the POS changefeed's live rule
--     (`sync_live_ingredient`) and the menu catalog's `ingredients[]` read, so
--     an archived ingredient stays on every till's mirror and recipes that
--     still use it keep deducting stock offline and online.
--   * `deleted_at` is untouched, so the name stays reserved by the partial
--     unique index `uq_org_ingredients_org_name (org_id, name) WHERE
--     deleted_at IS NULL`: creating a new ingredient with an archived one's
--     name is refused with "unarchive it instead".
--   * No movement is posted and no balance changes (the on_hand guard trigger
--     is not involved).
--
-- Additive only: two nullable columns (metadata-only ALTER, no rewrite) and a
-- consistency CHECK. RLS (`tenant_isolation`), the `sync_emit` trigger and the
-- updated_at trigger already cover the table; an archive is an UPDATE, so the
-- feed re-emits the ingredient as an upsert carrying the new additive
-- `archived_at` field (src/sync/pull/projection.rs).
--
-- SOURCE TABLES: none added (org_ingredients already emits type `ingredient`).

ALTER TABLE org_ingredients
    ADD COLUMN IF NOT EXISTS archived_at timestamptz NULL,
    ADD COLUMN IF NOT EXISTS archived_by uuid NULL REFERENCES users(id) ON DELETE SET NULL;

COMMENT ON COLUMN org_ingredients.archived_at IS
    'Set when the ingredient is archived (hidden from pickers/counts, kept for history). NULL = active. Independent of is_active and deleted_at.';
COMMENT ON COLUMN org_ingredients.archived_by IS
    'User who archived it; NULL when not archived or the user is gone.';

ALTER TABLE org_ingredients DROP CONSTRAINT IF EXISTS org_ingredients_archived_by_needs_archived_at;
ALTER TABLE org_ingredients
    ADD CONSTRAINT org_ingredients_archived_by_needs_archived_at
    CHECK (archived_at IS NOT NULL OR archived_by IS NULL);
