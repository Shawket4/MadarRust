-- Race backstop: at most one OPEN (draft/in_progress) stocktake per branch.
-- create_stocktake pre-checks this, but only a partial unique index makes it
-- race-proof against two concurrent opens (audit V12).
CREATE UNIQUE INDEX IF NOT EXISTS idx_stocktakes_one_open_per_branch
    ON stocktakes (branch_id)
    WHERE status IN ('draft', 'in_progress');
