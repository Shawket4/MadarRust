-- Hunt B-H1-1 (SC-9, S-162): a claim on an open shift is a request, and it
-- stays in the claimer's Requests once decided, as every other kind does.
-- `staff_open_shifts` keeps only the live claimer (a decline sets
-- claimed_by back to NULL), so the server forgot who was declined. This log
-- keeps every claim: written on claim, decision, the manager taking the shift
-- back (declined) and the claimer withdrawing it.
CREATE TABLE staff_open_shift_claims (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    open_shift_id uuid NOT NULL REFERENCES staff_open_shifts(id) ON DELETE CASCADE,
    employee_id   uuid NOT NULL REFERENCES employees(id) ON DELETE CASCADE,
    status        text NOT NULL DEFAULT 'pending',
    created_at    timestamptz NOT NULL DEFAULT now(),
    decided_at    timestamptz,
    decided_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    CONSTRAINT staff_open_shift_claims_status_chk
        CHECK (status IN ('pending', 'approved', 'declined', 'withdrawn'))
);
COMMENT ON TABLE staff_open_shift_claims IS
    'Every claim on an open shift and how it ended (SC-9): the claimer''s request history.';
COMMENT ON COLUMN staff_open_shift_claims.decided_by IS
    'The user who approved or declined it; NULL for a withdrawal (the claimer) or a row from before the log.';
-- One live claim per open shift, as staff_open_shifts.claimed_by allows.
CREATE UNIQUE INDEX staff_open_shift_claims_one_pending
    ON staff_open_shift_claims (open_shift_id) WHERE status = 'pending';
CREATE INDEX staff_open_shift_claims_employee_idx
    ON staff_open_shift_claims (employee_id, created_at DESC);

ALTER TABLE staff_open_shift_claims ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_open_shift_claims FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_open_shift_claims TO madar_app;
