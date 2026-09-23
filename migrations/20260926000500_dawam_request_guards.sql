-- Dawam RQ-11: overlapping live requests of the same kind are refused by the
-- database, so two sent at once can't both land. Windows are compared, not
-- just days: a late arrival runs from midnight to the agreed time, an early
-- departure from its time to the end of the day, an excuse is its window, and
-- leave and missions cover whole days.
-- Corrections are compared per shift instead (RQ-9).
--
-- Rows that already overlap an earlier live one are cancelled first, the
-- earliest kept, or the constraint could not be added.

UPDATE staff_requests r
   SET status = 'cancelled',
       decision_note = COALESCE(r.decision_note || ' · ', '') || 'Cancelled: overlapped an earlier request'
 WHERE r.kind <> 'correction'
   AND r.status IN ('pending', 'approved')
   AND EXISTS (
       SELECT 1 FROM staff_requests o
        WHERE o.user_id = r.user_id AND o.kind = r.kind AND o.id <> r.id
          AND o.status IN ('pending', 'approved')
          AND (o.created_at, o.id) < (r.created_at, r.id)
          AND tsrange(o.on_date + COALESCE(o.from_time, time '00:00'),
                      COALESCE(o.end_date, o.on_date) + COALESCE(o.to_time, time '24:00'), '[)')
           && tsrange(r.on_date + COALESCE(r.from_time, time '00:00'),
                      COALESCE(r.end_date, r.on_date) + COALESCE(r.to_time, time '24:00'), '[)'));

ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_no_overlap EXCLUDE USING gist (
    user_id WITH =,
    kind WITH =,
    tsrange(on_date + COALESCE(from_time, time '00:00'),
            COALESCE(end_date, on_date) + COALESCE(to_time, time '24:00'), '[)') WITH &&
) WHERE (kind <> 'correction' AND status IN ('pending', 'approved'));

-- The old one-request-per-kind-per-day index is replaced by the window
-- comparison above: two separate excuses on one day are allowed (RQ-11).
-- One live correction per shift (RQ-9) stays with
-- staff_requests_live_correction_unique.
DROP INDEX IF EXISTS staff_requests_live_unique;
