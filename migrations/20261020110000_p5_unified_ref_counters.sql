-- DB architecture Phase 5.3 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.3, L5):
-- one `ref_counters` table instead of order_ref_counters / ticket_ref_counters /
-- delivery_ref_counters (three identical per-(branch, business day) sequences).
--
-- EXPAND, this release:
--   * ref_counters (scope, branch_id, business_date) → last_seq, with the tenant
--     trio and flat RLS;
--   * backfill from the three tables: INSERT … ON CONFLICT DO UPDATE SET
--     last_seq = GREATEST(…) — idempotent, never lowers a counter;
--   * the writers (src/refs.rs) now take the next number from BOTH: they bump
--     the old table's row (whose row lock is the serialization point an older
--     binary in a rolling deploy also takes) to GREATEST(old, unified) + 1, and
--     write the same value into ref_counters. Two binaries can therefore never
--     mint the same ref for one branch and day.
-- Reconciliation: scripts/db_phase5_check.sql (max(last_seq) per key equal in
-- old and new, both ways).
-- CONTRACT (parked: migrations/parked/p5_ref_counters_contract.sql): once no
-- older binary runs, the writers use ref_counters alone and the three old
-- tables move to the archive schema.
SET LOCAL lock_timeout = '15s';

CREATE TABLE IF NOT EXISTS ref_counters (
    scope         text    NOT NULL CHECK (scope IN ('order', 'ticket', 'delivery')),
    org_id        uuid    NOT NULL,
    branch_id     uuid    NOT NULL,
    business_date date    NOT NULL,
    last_seq      integer NOT NULL,
    PRIMARY KEY (scope, branch_id, business_date),
    FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE
);

CREATE OR REPLACE FUNCTION fill_org_id_ref_counters() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.org_id IS NULL THEN
        SELECT b.org_id INTO NEW.org_id FROM branches b WHERE b.id = NEW.branch_id;
        IF NEW.org_id IS NULL THEN
            RAISE EXCEPTION 'ref_counters: no organization for branch %', NEW.branch_id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS fill_org_id ON ref_counters;
CREATE TRIGGER fill_org_id BEFORE INSERT ON ref_counters
    FOR EACH ROW EXECUTE FUNCTION fill_org_id_ref_counters();

ALTER TABLE ref_counters ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON ref_counters;
CREATE POLICY tenant_isolation ON ref_counters FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON ref_counters TO madar_app;

INSERT INTO ref_counters (scope, org_id, branch_id, business_date, last_seq)
SELECT 'order', b.org_id, c.branch_id, c.business_date, c.last_seq
  FROM order_ref_counters c JOIN branches b ON b.id = c.branch_id
ON CONFLICT (scope, branch_id, business_date)
DO UPDATE SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq);
INSERT INTO ref_counters (scope, org_id, branch_id, business_date, last_seq)
SELECT 'ticket', b.org_id, c.branch_id, c.business_date, c.last_seq
  FROM ticket_ref_counters c JOIN branches b ON b.id = c.branch_id
ON CONFLICT (scope, branch_id, business_date)
DO UPDATE SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq);
INSERT INTO ref_counters (scope, org_id, branch_id, business_date, last_seq)
SELECT 'delivery', b.org_id, c.branch_id, c.business_date, c.last_seq
  FROM delivery_ref_counters c JOIN branches b ON b.id = c.branch_id
ON CONFLICT (scope, branch_id, business_date)
DO UPDATE SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq);
