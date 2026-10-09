-- Mac E2E R-B3 (RU-10, AT-10): a public holiday is decided by any manager
-- who may publish a roster at some branch, so who decided it and WHEN are
-- recorded. decided_by exists; decided_at joins it. Same table: grants and
-- RLS already cover the new column.
ALTER TABLE staff_holidays ADD COLUMN decided_at timestamptz;
UPDATE staff_holidays SET decided_at = now() WHERE decision IS NOT NULL AND decided_at IS NULL;
