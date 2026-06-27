-- New org-scoped role: kitchen. Signs into a Kitchen Display device (PIN, like a
-- teller/waiter) to read the kitchen feed and bump lines; never holds a shift or
-- cash and cannot reach the POS (orders/payments) or settle tickets. Kept in its
-- OWN migration because a value added by `ALTER TYPE ... ADD VALUE` cannot be USED
-- later in the same transaction — the role-permission seed runs separately at startup.
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'kitchen';
