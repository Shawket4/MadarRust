-- A teller may hold only ONE open shift, ever — enforced at the database so no
-- race (e.g. concurrent opens at different branches, or a second device) can
-- bypass it. This is the backstop behind the application-level guards.
--
-- PRECONDITION: no teller may currently have more than one open shift, or this
-- CREATE INDEX will fail. Resolve any duplicates first by closing the extras
-- (dashboard / admin force-close) — see INVENTORY_PROD_MIGRATION_RUNBOOK.md.
-- We deliberately do NOT auto-close shifts in this migration: closing a shift
-- settles cash, so it must be a deliberate, audited operator action — never a
-- silent migration side effect.
--
-- Pre-check before running:
--   SELECT teller_id, count(*) FROM shifts WHERE status='open' GROUP BY 1 HAVING count(*) > 1;

CREATE UNIQUE INDEX idx_shifts_one_open_per_teller
    ON shifts (teller_id)
    WHERE status = 'open';
