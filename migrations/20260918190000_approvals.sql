-- Manager approvals (PERMISSIONS_ARCHITECTURE §4.2, phase 5).
--
-- A till asks for one when `decide` answers NeedsApproval (over a limit, not
-- the person's own sale, or a capability the owner lets them ask for). A
-- manager enters their PIN on the same device; the core checks their grants
-- offline and mints this record, which rides the queued op as `approval`.
-- At replay the server re-checks the approver held the act and stores it here.
CREATE TABLE IF NOT EXISTS approvals (
    id               uuid PRIMARY KEY,
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id        uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    device_id        uuid NULL,
    capability       text NOT NULL,
    subject_user_id  uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    approver_user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    method           text NOT NULL DEFAULT 'pin_local' CHECK (method IN ('pin_local')),
    amount_minor     bigint NULL,
    op               text NOT NULL,
    occurred_at      timestamptz NOT NULL,
    received_at      timestamptz NOT NULL DEFAULT now(),
    -- Did the approver hold the act when the server checked?
    verified         boolean NOT NULL,
    verification_error text NULL
);
CREATE INDEX IF NOT EXISTS approvals_org_time ON approvals (org_id, occurred_at DESC);

ALTER TABLE approvals ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS approvals_tenant ON approvals;
CREATE POLICY approvals_tenant ON approvals
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT ON approvals TO madar_app;
