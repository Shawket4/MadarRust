-- E2E B-TEAM-3 / RQ-F6 (AT-7, AT-10): cancelling a request used to overwrite
-- decided_by / decision_note, so an approved request that was later cancelled
-- lost who approved it and why. The cancellation gets its own who / when /
-- why; the decision's columns keep the approval. Same table: its grants and
-- RLS policy already cover the new columns.
ALTER TABLE staff_requests
    ADD COLUMN cancelled_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN cancelled_at timestamptz,
    ADD COLUMN cancel_note  text;

-- Rows cancelled before this: their decided_* columns hold the CANCELLATION
-- (the approver, if any, is already lost). Copy it to where it belongs; the
-- decided_* columns are left as they are, since for a request cancelled while
-- pending they are the only record of who cancelled it on old clients.
UPDATE staff_requests
   SET cancelled_by = decided_by, cancelled_at = COALESCE(decided_at, updated_at),
       cancel_note = decision_note
 WHERE status = 'cancelled' AND cancelled_at IS NULL;
