-- Attach shifts to a till (the drawer), and move the "one open shift" uniqueness
-- from per-branch to per-till. A teller still holds at most one open shift
-- (idx_shifts_one_open_per_teller, kept), but a branch may now have several open
-- shifts at once — one per till.

ALTER TABLE shifts ADD COLUMN till_id uuid REFERENCES tills(id);

-- Backfill: give every branch a default "Till 1", then point all existing shifts
-- at their branch's default till. Today there is at most one open shift per
-- branch, so this mapping is unambiguous. A default till is created for EVERY
-- branch (even ones with no shift history) so legacy/offline opens that omit a
-- till_id always have a fallback.
INSERT INTO tills (org_id, branch_id, name, is_default, is_active)
SELECT b.org_id, b.id, 'Till 1', true, true
FROM branches b;

UPDATE shifts s
SET till_id = t.id
FROM tills t
WHERE t.branch_id = s.branch_id AND t.is_default = true;

-- Safety net so every shift always lands on a drawer: a BEFORE INSERT trigger
-- fills till_id from the branch's default till (lazily creating one if the
-- branch has none) whenever a row is inserted without it. The app's open-shift
-- path already resolves a till explicitly (clean errors + per-till continuity);
-- this covers direct inserts (admin tooling, replay, tests) so the NOT NULL
-- column below can never block them. For app inserts (till_id already set) the
-- trigger is a no-op.
CREATE OR REPLACE FUNCTION shifts_fill_default_till() RETURNS trigger AS $$
DECLARE
    v_till uuid;
    v_org  uuid;
BEGIN
    IF NEW.till_id IS NOT NULL THEN
        RETURN NEW;
    END IF;
    SELECT id INTO v_till FROM tills
        WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
        LIMIT 1;
    IF v_till IS NULL THEN
        SELECT org_id INTO v_org FROM branches WHERE id = NEW.branch_id;
        INSERT INTO tills (org_id, branch_id, name, is_default, is_active)
            VALUES (v_org, NEW.branch_id, 'Till 1', true, true)
            ON CONFLICT DO NOTHING;
        SELECT id INTO v_till FROM tills
            WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
            LIMIT 1;
    END IF;
    NEW.till_id := v_till;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_shifts_fill_default_till
    BEFORE INSERT ON shifts
    FOR EACH ROW EXECUTE FUNCTION shifts_fill_default_till();

ALTER TABLE shifts ALTER COLUMN till_id SET NOT NULL;

CREATE INDEX idx_shifts_till ON shifts (till_id);

-- The drawer is now the concurrency unit: one open shift per till, not per branch.
-- Two historical one-open-per-branch indexes existed (full_schema + audit_fixes);
-- drop both.
DROP INDEX IF EXISTS idx_shifts_one_open_per_branch;
DROP INDEX IF EXISTS idx_shifts_branch_one_open;
CREATE UNIQUE INDEX idx_shifts_one_open_per_till ON shifts (till_id) WHERE status = 'open';
