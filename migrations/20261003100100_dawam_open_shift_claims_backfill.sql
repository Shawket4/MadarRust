-- Hunt B-H1-1: the claims the open shifts still remember, into the log.
-- claimed → pending, filled → approved, and a shift taken back while its
-- claim waited (cancelled with a claimer) → declined. A claim declined before
-- the log is gone (the decline cleared claimed_by). No decision time was
-- kept, so decided_at stays NULL on these rows. Idempotent: a shift already
-- in the log is skipped.
INSERT INTO staff_open_shift_claims
    (org_id, open_shift_id, employee_id, status, created_at, decided_by)
SELECT o.org_id, o.id, o.claimed_by,
       CASE o.status WHEN 'claimed' THEN 'pending'
                     WHEN 'filled' THEN 'approved'
                     ELSE 'declined' END,
       COALESCE(o.claimed_at, o.created_at),
       CASE WHEN o.status = 'claimed' THEN NULL ELSE o.decided_by END
  FROM staff_open_shifts o
 WHERE o.claimed_by IS NOT NULL
   AND o.status IN ('claimed', 'filled', 'cancelled')
   AND NOT EXISTS (SELECT 1 FROM staff_open_shift_claims c WHERE c.open_shift_id = o.id);
