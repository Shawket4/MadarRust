-- Close-till reconciliation per payment method (decision 11) + branch till
-- settings (decision 8: old bills; standard float moves from the removed drawer
-- entity to the branch). TILLS_CONTRACT.md §1.2.

CREATE TABLE till_reconciliations (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    till_id               uuid NOT NULL REFERENCES tills(id) ON DELETE CASCADE,
    method                text NOT NULL CHECK (btrim(method) <> ''),   -- same string as order_payments.method
    payment_method_id     uuid NULL REFERENCES org_payment_methods(id) ON DELETE SET NULL,
    is_cash               boolean NOT NULL,
    system_total          integer NOT NULL,      -- snapshot at close (cash row: closing_cash_system)
    current_system_total  integer NOT NULL,      -- recomputed when a late replay lands on the closed till
    order_count           integer NOT NULL DEFAULT 0,
    status                text NOT NULL CHECK (status IN ('checked','disagreed','unreviewed')),
    declared_amount       integer NULL,
    note                  text NULL CHECK (note IS NULL OR btrim(note) <> ''),
    reconciled_by         uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    reconciled_at         timestamptz NOT NULL DEFAULT now(),
    created_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT uq_till_reconciliations_method UNIQUE (till_id, method),
    CONSTRAINT till_reconciliations_disagreed_has_amount CHECK (status <> 'disagreed' OR declared_amount IS NOT NULL),
    CONSTRAINT till_reconciliations_noncash_disagreed_has_note CHECK (status <> 'disagreed' OR is_cash OR note IS NOT NULL)
);
CREATE INDEX idx_till_reconciliations_disagreed ON till_reconciliations (till_id) WHERE status = 'disagreed';
ALTER TABLE till_reconciliations ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON till_reconciliations
    USING (EXISTS (SELECT 1 FROM tills p WHERE p.id = till_reconciliations.till_id));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE till_reconciliations TO madar_app;

ALTER TABLE branches
    ADD COLUMN old_bill_hours smallint NOT NULL DEFAULT 3
        CONSTRAINT branches_old_bill_hours_range CHECK (old_bill_hours BETWEEN 1 AND 168),
    ADD COLUMN standard_float integer NULL
        CONSTRAINT branches_standard_float_nonneg CHECK (standard_float IS NULL OR standard_float >= 0);
COMMENT ON COLUMN branches.old_bill_hours IS
    'An open bill older than this many hours is flagged as old (open-bills notice, Z report).';
COMMENT ON COLUMN branches.standard_float IS
    'The cash that should be in a drawer at the start of a till, in minor units. NULL = not set.';

-- Backfill from the archived default drawer entity (trg_branches_updated_at is
-- held off so branch history is not touched by a settings move).
ALTER TABLE branches DISABLE TRIGGER trg_branches_updated_at;
UPDATE branches b
   SET standard_float = e.standard_float
  FROM archive.till_entities e
 WHERE e.branch_id = b.id AND e.is_default AND e.deleted_at IS NULL
   AND e.standard_float IS NOT NULL;
ALTER TABLE branches ENABLE TRIGGER trg_branches_updated_at;

-- Invariant (§1.3): every archived live default drawer with a float moved its
-- float to its branch (deleted branches included — nothing is dropped).
DO $$
BEGIN
    IF (SELECT count(*) FROM branches WHERE standard_float IS NOT NULL)
       <> (SELECT count(*) FROM archive.till_entities e JOIN branches b ON b.id = e.branch_id
            WHERE e.is_default AND e.deleted_at IS NULL AND e.standard_float IS NOT NULL) THEN
        RAISE EXCEPTION 'tills-rework invariant: branches.standard_float backfill does not match archived default drawers';
    END IF;
END $$;
