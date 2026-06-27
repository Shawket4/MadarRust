-- New org-scoped role: waiter. Takes dine-in orders and fires them to the kitchen
-- as unpaid open tickets; never holds a shift or cash. Kept in its OWN migration
-- because a value added by `ALTER TYPE ... ADD VALUE` cannot be USED later in the
-- same transaction — the role-permission seed runs separately at startup.
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'waiter';
