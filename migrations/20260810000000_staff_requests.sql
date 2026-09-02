-- Staff requests — one table for every "may I?" an employee asks.
--
-- Replaces `leave_requests`, `late_passes`, and `missions`, which were three
-- copies of the same status machine. Adding early departures and mid-shift
-- permissions would have made five. Instead there is ONE table, one approval
-- path, one inbox.
--
-- THE UNIFYING IDEA: every kind is an EXCUSED WINDOW inside a day (or a span of
-- days). `[from_time, to_time]` is that window, open-ended on either side:
--
--   kind             on_date/end_date   from_time      to_time        extra
--   ───────────────────────────────────────────────────────────────────────────
--   leave            span               —              —              leave_type_id
--   late_arrival     single day         —              excused UNTIL
--   early_departure  single day         excused FROM   —
--   excuse           single day         window start   window end
--   mission          span               optional       optional       title, location
--
-- That is why a late arrival and a two-hour errand need no separate machinery:
-- both are "this part of the day is forgiven", differing only in which end is
-- open. The attendance classifier reads one shape (see `staff::penalties`).

CREATE TABLE staff_requests (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind          text NOT NULL,

    -- The day the request applies to; `end_date` extends it into a span.
    on_date       date NOT NULL,
    end_date      date,
    -- The excused window within the day, in the BRANCH's local wall clock.
    -- NULL on either side means "open to the shift boundary".
    from_time     time,
    to_time       time,

    -- `leave` only.
    leave_type_id uuid REFERENCES leave_types(id) ON DELETE RESTRICT,
    is_half_day   boolean NOT NULL DEFAULT false,

    -- `mission` only.
    title         text,
    location      text,

    reason        text,
    status        text NOT NULL DEFAULT 'pending',

    -- Whether the excused time is PAID. NULL until decided, then resolved from
    -- `attendance_settings.excused_time_paid_default` — which the approver may
    -- override on this one request.
    is_paid       boolean,

    decided_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at    timestamptz,
    decision_note text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT staff_requests_kind_chk CHECK (
        kind IN ('leave', 'late_arrival', 'early_departure', 'excuse', 'mission')
    ),
    CONSTRAINT staff_requests_status_chk CHECK (
        status IN ('pending', 'approved', 'rejected', 'cancelled')
    ),
    CONSTRAINT staff_requests_span_ordered CHECK (end_date IS NULL OR end_date >= on_date),
    CONSTRAINT staff_requests_window_ordered CHECK (
        from_time IS NULL OR to_time IS NULL OR to_time > from_time
    ),
    -- Each kind's required shape, enforced at the door so a malformed request can
    -- never reach the classifier and silently forgive (or fail to forgive) a day.
    CONSTRAINT staff_requests_shape_chk CHECK (
        CASE kind
            WHEN 'leave' THEN
                leave_type_id IS NOT NULL AND end_date IS NOT NULL
                AND from_time IS NULL AND to_time IS NULL
            WHEN 'late_arrival' THEN
                to_time IS NOT NULL AND from_time IS NULL AND end_date IS NULL
                AND leave_type_id IS NULL
            WHEN 'early_departure' THEN
                from_time IS NOT NULL AND to_time IS NULL AND end_date IS NULL
                AND leave_type_id IS NULL
            WHEN 'excuse' THEN
                from_time IS NOT NULL AND to_time IS NOT NULL AND end_date IS NULL
                AND leave_type_id IS NULL
            WHEN 'mission' THEN
                title IS NOT NULL AND end_date IS NOT NULL AND leave_type_id IS NULL
            ELSE false
        END
    ),
    -- A half day is a single-day leave concept.
    CONSTRAINT staff_requests_half_day_chk CHECK (
        NOT is_half_day OR (kind = 'leave' AND on_date = end_date)
    ),
    CONSTRAINT staff_requests_decision_stamped CHECK (
        status IN ('pending', 'cancelled') OR decided_at IS NOT NULL
    )
);

CREATE INDEX staff_requests_user_idx ON staff_requests (user_id, on_date DESC);
CREATE INDEX staff_requests_org_idx  ON staff_requests (org_id, status, on_date DESC);
-- The classifier's hot lookup: "what is forgiven for this person on this day?"
CREATE INDEX staff_requests_live_idx
    ON staff_requests (user_id, on_date, COALESCE(end_date, on_date))
    WHERE status = 'approved';
-- At most one live request of a kind per person per day. Two approved late
-- arrivals for the same morning would be ambiguous about which deadline holds.
CREATE UNIQUE INDEX staff_requests_live_unique
    ON staff_requests (user_id, kind, on_date)
    WHERE status IN ('pending', 'approved') AND kind <> 'leave';

-- ── Migrate the three predecessors ───────────────────────────────────────────
-- Dev is the only environment holding these rows, so this is a straight lift.

INSERT INTO staff_requests (
    id, org_id, user_id, kind, on_date, end_date, leave_type_id, is_half_day,
    reason, status, decided_by, decided_at, decision_note, created_at, updated_at
)
SELECT id, org_id, user_id, 'leave', start_date, end_date, leave_type_id,
       is_half_day, reason, status, decided_by, decided_at, decision_note,
       created_at, updated_at
  FROM leave_requests;

INSERT INTO staff_requests (
    id, org_id, user_id, kind, on_date, to_time, reason, status,
    decided_by, decided_at, decision_note, created_at, updated_at
)
SELECT id, org_id, user_id, 'late_arrival', on_date, expected_arrival_time,
       reason, status, decided_by, decided_at, decision_note, created_at, updated_at
  FROM late_passes;

-- Missions carry instants; the window collapses to the local date span. The
-- times are dropped deliberately — a mission excuses whole days, and keeping a
-- UTC time-of-day here would be read as branch-local by the classifier.
INSERT INTO staff_requests (
    id, org_id, user_id, kind, on_date, end_date, title, location, reason,
    status, decided_by, decided_at, decision_note, created_at, updated_at
)
SELECT id, org_id, user_id, 'mission', starts_at::date, ends_at::date,
       title, location, description, status,
       decided_by, decided_at, decision_note, created_at, updated_at
  FROM missions;

DROP TABLE leave_requests;
DROP TABLE late_passes;
DROP TABLE missions;

-- ── Settings: the org-level default for excused time ─────────────────────────
ALTER TABLE attendance_settings
    -- Whether an approved mid-shift excuse / early departure is PAID by default.
    -- The approver may override it per request (staff_requests.is_paid).
    ADD COLUMN IF NOT EXISTS excused_time_paid_default boolean NOT NULL DEFAULT true;

-- ── RLS ──────────────────────────────────────────────────────────────────────
ALTER TABLE staff_requests ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_requests FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE staff_requests TO sufrix;
