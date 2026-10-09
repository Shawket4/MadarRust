-- Owner decision D5 (24 Sep 2026): how a confirmed cover is paid is a rule.
--   minute_rate — the coverer's plain minute rate: day rate ÷ 8 h × the
--                 minutes covered (spec CV-4). The default.
--   full_block  — the covered block paid as a full day (the old behaviour).
-- The business row sets it; a branch row may override it (NULL = the
-- business's), like every other branch-overridable rule. An existing
-- business row stays NULL and reads as the built-in default (minute_rate);
-- the resolver coalesces, so the business-complete check is unchanged.
-- Same table: its grants and RLS policy already cover the new column.
ALTER TABLE attendance_settings ADD COLUMN cover_pay_mode text;
ALTER TABLE attendance_settings ADD CONSTRAINT attendance_settings_cover_pay_mode_chk
    CHECK (cover_pay_mode IS NULL OR cover_pay_mode IN ('minute_rate', 'full_block'));
