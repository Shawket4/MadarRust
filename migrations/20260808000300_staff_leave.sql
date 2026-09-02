-- Staff & attendance — part 4 of 5: leave, late passes, missions.
--
-- Three request kinds share one status machine
-- (pending → approved | rejected, or cancelled by the requester):
--   * leave_requests — whole days off against a quota'd leave type.
--   * late_passes    — permission to arrive late on one date, so the grace-period
--                      check is waived up to the agreed arrival time.
--   * missions       — sanctioned off-site work; the employee is present for
--                      payroll purposes without a geofenced check-in.
--
-- All three are read by the attendance classifier: an approved leave day becomes
-- 'on_leave' rather than 'absent', an approved mission counts as worked, and an
-- approved late pass suppresses the late penalty.

CREATE TABLE leave_types (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name              text NOT NULL,
    -- Unpaid leave still excuses the absence, but is not paid in the payslip.
    is_paid           boolean NOT NULL DEFAULT true,
    -- NULL = uncapped (e.g. unpaid leave).
    annual_quota_days numeric(6,2),
    requires_approval boolean NOT NULL DEFAULT true,
    is_active         boolean NOT NULL DEFAULT true,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT leave_types_name_len   CHECK (char_length(name) BETWEEN 1 AND 120),
    CONSTRAINT leave_types_quota_nonneg
        CHECK (annual_quota_days IS NULL OR annual_quota_days >= 0)
);

CREATE UNIQUE INDEX leave_types_org_name_key ON leave_types (org_id, lower(name));

CREATE TABLE leave_balances (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id           uuid NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    leave_type_id     uuid NOT NULL REFERENCES leave_types(id) ON DELETE CASCADE,
    year              integer NOT NULL,
    entitled_days     numeric(6,2) NOT NULL DEFAULT 0,
    -- Incremented when a request is APPROVED, decremented if it is later
    -- cancelled. Never derived on the fly: the entitlement can change mid-year.
    used_days         numeric(6,2) NOT NULL DEFAULT 0,
    carried_over_days numeric(6,2) NOT NULL DEFAULT 0,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT leave_balances_year_sane CHECK (year BETWEEN 2000 AND 2200),
    CONSTRAINT leave_balances_nonneg
        CHECK (entitled_days >= 0 AND used_days >= 0 AND carried_over_days >= 0)
);

CREATE UNIQUE INDEX leave_balances_unique
    ON leave_balances (user_id, leave_type_id, year);

CREATE TABLE leave_requests (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    leave_type_id uuid NOT NULL REFERENCES leave_types(id) ON DELETE RESTRICT,
    start_date    date NOT NULL,
    end_date      date NOT NULL,
    -- A single-day request taken as half a day.
    is_half_day   boolean NOT NULL DEFAULT false,
    reason        text,
    status        text NOT NULL DEFAULT 'pending',
    decided_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at    timestamptz,
    decision_note text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT leave_requests_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected', 'cancelled')),
    CONSTRAINT leave_requests_dates_ordered CHECK (end_date >= start_date),
    CONSTRAINT leave_requests_half_day_single
        CHECK (NOT is_half_day OR start_date = end_date),
    CONSTRAINT leave_requests_decision_stamped
        CHECK (status IN ('pending', 'cancelled') OR decided_at IS NOT NULL)
);

CREATE INDEX leave_requests_user_idx   ON leave_requests (user_id, start_date DESC);
CREATE INDEX leave_requests_org_idx    ON leave_requests (org_id, status, start_date DESC);
-- The attendance classifier's hot lookup: "is this person on approved leave today?"
CREATE INDEX leave_requests_span_idx   ON leave_requests (user_id, start_date, end_date)
    WHERE status = 'approved';

CREATE TABLE late_passes (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                 uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id                uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    on_date                date NOT NULL,
    -- Grace is extended up to this local wall-clock time on `on_date`.
    expected_arrival_time  time NOT NULL,
    reason                 text,
    status                 text NOT NULL DEFAULT 'pending',
    decided_by             uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at             timestamptz,
    decision_note          text,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT late_passes_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected', 'cancelled'))
);

-- At most one live pass per person per day; rejected/cancelled ones may pile up.
CREATE UNIQUE INDEX late_passes_live_unique
    ON late_passes (user_id, on_date)
    WHERE status IN ('pending', 'approved');
CREATE INDEX late_passes_org_idx ON late_passes (org_id, status, on_date DESC);

CREATE TABLE missions (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The branch the mission is charged against for attendance reporting.
    branch_id     uuid REFERENCES branches(id) ON DELETE SET NULL,
    title         text NOT NULL,
    location      text,
    description   text,
    starts_at     timestamptz NOT NULL,
    ends_at       timestamptz NOT NULL,
    status        text NOT NULL DEFAULT 'pending',
    decided_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at    timestamptz,
    decision_note text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT missions_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected', 'cancelled')),
    CONSTRAINT missions_title_len      CHECK (char_length(title) BETWEEN 1 AND 200),
    CONSTRAINT missions_span_ordered   CHECK (ends_at > starts_at)
);

CREATE INDEX missions_user_idx ON missions (user_id, starts_at DESC);
CREATE INDEX missions_org_idx  ON missions (org_id, status, starts_at DESC);

-- ── RLS ──────────────────────────────────────────────────────────────────────
ALTER TABLE leave_types    ENABLE ROW LEVEL SECURITY;
ALTER TABLE leave_balances ENABLE ROW LEVEL SECURITY;
ALTER TABLE leave_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE late_passes    ENABLE ROW LEVEL SECURITY;
ALTER TABLE missions       ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON leave_types FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON leave_balances FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON leave_requests FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON late_passes FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON missions FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE leave_types    TO sufrix;
GRANT ALL ON TABLE leave_balances TO sufrix;
GRANT ALL ON TABLE leave_requests TO sufrix;
GRANT ALL ON TABLE late_passes    TO sufrix;
GRANT ALL ON TABLE missions       TO sufrix;
