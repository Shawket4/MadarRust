-- A customer at a table can order for themselves.
--
-- The QR on table 7 has existed since the qr_card module was written, and it
-- has never worked the way a customer expects. It encodes
-- `<shop>/order?branch=…&table=…`, the ordering page ignores the table
-- entirely, and the customer is then asked which branch they are in — while
-- sitting in it — which channel, for their phone number, and for their
-- location. Everything the QR already knew, asked again.
--
-- What a scan should be is one thing: this table's menu, add, send. The order
-- is a DINE-IN BILL ON THAT TABLE, not a delivery order with a table written
-- on it. That choice decides almost everything downstream and it is worth
-- stating why:
--
--   * The floor shows table 7 as occupied, because it is. A delivery order
--     leaves the room looking empty while four people sit in it.
--   * A second scan by the same party ADDS A ROUND to the same bill, which is
--     how ordering at a table actually goes. Separate orders would hand the
--     teller four things to reconcile at the end of one meal.
--   * It settles at the till through `settle_open_ticket`, so it is priced
--     `dine_in` and carries the service charge — which the owner ruled is
--     dine-in only. A delivery order books as takeaway and would silently
--     drop the charge on exactly the sales that earn it.
--   * The kitchen ticket, the readiness, the void path and the refund path
--     are the ones that already exist. Nothing here is a parallel pipeline.
--
-- ── Who opened the bill ─────────────────────────────────────────────────────
--
-- `open_tickets.opened_by` is NOT NULL and references a user, because until
-- now a bill was always opened by a member of staff. A customer is not one.
--
-- The alternative considered and rejected was making the column nullable. It
-- reads well in the abstract — nobody on the staff opened this — but
-- `opened_by` is the actor every downstream guard keys on: the void's
-- attribution, the floor `Hand`, the waiter column, the replay envelope. A
-- NULL would have to be answered for at each of those, and the answer at each
-- of them is the same sentence: "the customer did it". So the row says that
-- once, here, instead of in five places.
--
-- It is a real principal, not a placeholder. Something opened this bill, and
-- "the customer, through the code on table 7" is a truthful name for it. The
-- row can hold no password and no PIN, so it can never be signed in as.

ALTER TABLE users
    ADD COLUMN is_guest_principal boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN users.is_guest_principal IS
    'This row is not a person. It is the actor a self-service order is
     attributed to — one per organisation, created on first use. Excluded from
     every staff list and every sign-in path; holds no password and no PIN.';

-- Cannot be signed in as, by construction rather than by remembering to check.
ALTER TABLE users
    ADD CONSTRAINT users_guest_principal_has_no_credentials
        CHECK (NOT is_guest_principal OR (password_hash IS NULL AND pin_hash IS NULL));

-- And the other half of the same sentence. `chk_login_method` has always said
-- every user must carry a password or a PIN — a reasonable rule for a table
-- whose every row was an account, and exactly wrong for a row that is not one.
-- Restated: everyone who CAN sign in has a way to, and the guest principal is
-- excused because it may never sign in at all. The two constraints together
-- leave no row that is both credential-less and login-able.
ALTER TABLE users
    DROP CONSTRAINT chk_login_method,
    ADD CONSTRAINT chk_login_method
        CHECK (is_guest_principal OR password_hash IS NOT NULL OR pin_hash IS NOT NULL);

-- One per organisation. `ON CONFLICT DO NOTHING` against this is what makes
-- "create on first use" safe when two people scan two tables in the same
-- second.
CREATE UNIQUE INDEX idx_users_one_guest_principal_per_org
    ON users (org_id)
    WHERE is_guest_principal AND deleted_at IS NULL;

-- ── Where the order came from ───────────────────────────────────────────────
--
-- A bill opened by a scan is not a bill a waiter opened, and a shop wants to
-- know which is which — for the same reason `order_type` exists. NULL is every
-- bill opened by staff, which is all of them before today.
ALTER TABLE open_tickets
    ADD COLUMN opened_via text
        CHECK (opened_via IS NULL OR opened_via IN ('qr_table'));

COMMENT ON COLUMN open_tickets.opened_via IS
    'How the bill was started. NULL = a member of staff opened it (every bill
     before 2026-09-12). `qr_table` = a customer scanned the code on their
     table and ordered for themselves.';

-- The floor and the Bills list both want "is this one of ours or one of
-- theirs" on a live bill, and both read the live set constantly.
CREATE INDEX idx_open_tickets_opened_via ON open_tickets (branch_id, opened_via)
    WHERE status = 'open' AND opened_via IS NOT NULL;
