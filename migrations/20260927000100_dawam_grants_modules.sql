-- Dawam Phase A: the tenant role's grants, and modules that start switched off.
--
-- Grants (audit B1 / B-1). Tenant requests run as `madar_app`
-- (src/db.rs). The Dawam migrations granted their new tables to the legacy
-- `sufrix` role only, or to nobody, so on a database without default
-- privileges (a restored prod copy) every Dawam request and every tagged till
-- pay-out failed with "permission denied". Each table is named here
-- explicitly; the test `dawam_grants_are_explicit` revokes everything and
-- replays this file to prove it.
GRANT SELECT, INSERT, UPDATE, DELETE ON
    staff_devices, attendance_pings, attendance_flags, staff_week_publications,
    staff_open_shifts, staff_swaps, staff_holidays, staff_suggestion_events,
    expense_advances, staff_notifications, push_devices, staff_coverage_needs,
    staff_suggestion_cache,
    employees, employee_branches,
    -- The August staff module's tables the re-keying touched, re-asserted.
    attendance_records, attendance_settings, staff_requests, leave_types,
    leave_balances, payroll_deductions, payroll_bonuses, payroll_periods, payslips,
    salary_advances, staff_schedules, staff_schedule_overrides, staff_documents,
    work_shifts, departments
TO madar_app;
GRANT EXECUTE ON FUNCTION dawam_night_minutes(timestamptz, timestamptz, text, time, time) TO madar_app;

-- Sign-in codes are read before anyone knows the org, by the owner pool only
-- (src/staff/dawam/signin.rs). The tenant role never sees another business's
-- live codes (B-9).
REVOKE ALL ON TABLE staff_otp FROM madar_app;
REVOKE ALL ON TABLE staff_otp FROM PUBLIC;
-- And row security with no policy: were a grant ever to come back, the tenant
-- role would still see no row. The owner pool (the table's owner) is unaffected.
ALTER TABLE staff_otp ENABLE ROW LEVEL SECURITY;

-- Modules (owner decision 2026-09-23). Existing businesses did not buy Dawam:
-- they get POS only, and Dawam is switched on per org by Madar (a super
-- admin). An org whose modules still hold exactly the old default was never
-- set by hand, so it goes back to POS; one set deliberately (`{dawam}`, a
-- Dawam-only customer) is left alone. The non-empty CHECK stays.
ALTER TABLE organizations ALTER COLUMN modules SET DEFAULT '{pos}';
UPDATE organizations SET modules = '{pos}'
 WHERE modules @> '{pos,dawam}' AND modules <@ '{pos,dawam}';
