-- Categories previously had a `display_order` column (added in the initial
-- schema, dropped in 20260613006000 as unused dead weight — no UI ever set
-- it). Custom drag-and-drop ordering brings it back, this time actually
-- wired to a UI (PUT /categories/order) and to the POS via /sync/pull.
--
-- Backfill with the current effective order (by name, per org) so switching
-- category lists to ORDER BY display_order is a no-op until an org
-- explicitly drags-and-drops a new order.
ALTER TABLE categories ADD COLUMN IF NOT EXISTS display_order integer NOT NULL DEFAULT 0;

UPDATE categories c
SET display_order = ranked.rn
FROM (
    SELECT id, row_number() OVER (PARTITION BY org_id ORDER BY name ASC, id ASC) - 1 AS rn
    FROM categories
    WHERE deleted_at IS NULL
) ranked
WHERE c.id = ranked.id AND c.display_order = 0;
