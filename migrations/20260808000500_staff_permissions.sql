-- Staff & attendance — new permission resources.
--
-- Being an employee is NOT a role: it is a `staff_profiles` row. What someone may
-- do about staff data is decided entirely by these resources, which is why no
-- `user_role` enum value was added.
--
--   staff        — the employee directory, departments, documents (salary lives here)
--   work_shifts  — shift templates + roster assignment  (NOT `shifts`, which is
--                  the teller cash-drawer session)
--   attendance   — the attendance ledger, manual entry/correction, settings
--   leave        — leave types/balances/requests, late passes, missions, approvals
--   payroll      — deductions, bonuses, advances, periods, payslips
--
-- Self-service is deliberately absent: `/staff/me/*` is always own-row scoped and
-- needs no grant, so an employee can clock in and read their own payslip without
-- being able to see anyone else's.
--
-- Kept in its own migration and NOT referenced in-file: `ALTER TYPE ... ADD VALUE`
-- values cannot be used in the transaction that adds them. The role_permissions
-- rows are seeded at startup by `permissions::seeder`.
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'staff';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'work_shifts';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'attendance';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'leave';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'payroll';
