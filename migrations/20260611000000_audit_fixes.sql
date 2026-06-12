-- ─────────────────────────────────────────────────────────────
-- Audit fixes migration (2026-06-11)
-- ─────────────────────────────────────────────────────────────

-- ── 1. Prevent two open shifts for the same branch ───────────
-- Replaces the application-level EXISTS check with an atomic DB constraint.
-- If a concurrent INSERT races past the check, the DB will reject it.
CREATE UNIQUE INDEX IF NOT EXISTS idx_shifts_branch_one_open
    ON shifts (branch_id)
    WHERE status = 'open';

-- ── 2. Remove duplicate indexes ──────────────────────────────
-- idx_orders_branch and idx_orders_branch_id are identical; keep the _id one.
DROP INDEX IF EXISTS idx_orders_branch;
-- idx_orders_shift and idx_orders_shift_id are identical; keep the _id one.
DROP INDEX IF EXISTS idx_orders_shift;
-- idx_order_payments_order and idx_order_payments_order_id are identical.
DROP INDEX IF EXISTS idx_order_payments_order;

-- ── 3. Add missing composite + covering indexes ───────────────
-- Branch-level reporting (used in reports handler, already has branch_id alone).
CREATE INDEX IF NOT EXISTS idx_orders_branch_created
    ON orders (branch_id, created_at DESC);

-- Shift reconciliation queries.
CREATE INDEX IF NOT EXISTS idx_orders_shift_created
    ON orders (shift_id, created_at DESC)
    WHERE shift_id IS NOT NULL;

-- Status-filtered summaries.
CREATE INDEX IF NOT EXISTS idx_orders_status_created
    ON orders (status, created_at DESC);

-- Menu item → category joins (missing entirely).
CREATE INDEX IF NOT EXISTS idx_menu_items_category
    ON menu_items (category_id)
    WHERE category_id IS NOT NULL AND deleted_at IS NULL;

-- ── 4. Voided order integrity CHECK constraints ───────────────
-- Enforce: if voided_at is set → status must be 'voided' AND voided_by must exist.
ALTER TABLE orders DROP CONSTRAINT IF EXISTS chk_orders_voided_consistency;
ALTER TABLE orders ADD CONSTRAINT chk_orders_voided_consistency CHECK (
    (voided_at IS NULL AND voided_by IS NULL)
    OR
    (voided_at IS NOT NULL AND voided_by IS NOT NULL AND status = 'voided')
);

-- ── 5. Partial unique index on users.email for soft-deleted rows ─
-- Prevents two active users sharing the same email while allowing
-- deleted users to keep their old email on record.
-- Drop the broad unique constraint; the partial index below replaces it.
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_email_key;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_email_active
    ON users (email)
    WHERE deleted_at IS NULL AND email IS NOT NULL;

-- ── 6. Price epoch date ordering constraint ───────────────────
ALTER TABLE menu_item_price_epochs DROP CONSTRAINT IF EXISTS chk_price_epoch_dates;
ALTER TABLE menu_item_price_epochs ADD CONSTRAINT chk_price_epoch_dates CHECK (
    effective_until IS NULL OR effective_until > effective_from
);

ALTER TABLE bundle_price_epochs DROP CONSTRAINT IF EXISTS chk_bundle_epoch_dates;
ALTER TABLE bundle_price_epochs ADD CONSTRAINT chk_bundle_epoch_dates CHECK (
    effective_until IS NULL OR effective_until > effective_from
);
