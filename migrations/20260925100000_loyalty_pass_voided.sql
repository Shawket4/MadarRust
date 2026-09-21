-- A retired card must SAY it is retired (design §2.7, §2.8).
--
-- When a membership ends without the person being erased — the loser of a
-- merge, or a customer leaving the programme — the Apple pass in their wallet
-- used to simply stop updating: the web service answered 404 for a deleted
-- member, so the device kept showing the last balance for ever. Apple's model
-- for "this pass is over" is a pass served with `"voided": true`; the device
-- greys it out and stops presenting its barcode.
--
-- `pass_voided_at` marks a soft-deleted membership whose pass is to be served
-- voided. While it is set the row keeps its `apple_auth_token` and its device
-- registrations, because a device has to authenticate to fetch the voided copy.
-- An ERASED member has neither (see `loyalty::model::forget`), and stays a 401.
ALTER TABLE loyalty_customers
    ADD COLUMN IF NOT EXISTS pass_voided_at timestamptz NULL;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'loyalty_customers_voided_is_deleted') THEN
        ALTER TABLE loyalty_customers ADD CONSTRAINT loyalty_customers_voided_is_deleted
            CHECK (pass_voided_at IS NULL OR deleted_at IS NOT NULL);
    END IF;
END $$;

-- Who changed a customer's identity, and from what to what (design §4.4). The
-- phone's own trail is `customer_phone_history`; this is the act: a rename, a
-- replaced phone, a self-service combine. `actor_user IS NULL` with
-- `actor_kind = 'customer'` is the customer acting for themself from a
-- verified device. The old/new values are personal data: a PDPL erase deletes
-- the customer's rows here.
CREATE TABLE IF NOT EXISTS customer_identity_audit (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    customer_id uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    -- 'name' | 'phone' | 'combine'
    kind        text NOT NULL,
    -- 'customer' | 'staff'
    actor_kind  text NOT NULL,
    actor_user  uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    old_value   text NULL,
    new_value   text NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT customer_identity_audit_kind CHECK (kind IN ('name', 'phone', 'combine')),
    CONSTRAINT customer_identity_audit_actor CHECK (actor_kind IN ('customer', 'staff'))
);
CREATE INDEX IF NOT EXISTS customer_identity_audit_customer
    ON customer_identity_audit (customer_id, created_at DESC);
ALTER TABLE customer_identity_audit ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS customer_identity_audit_tenant ON customer_identity_audit;
CREATE POLICY customer_identity_audit_tenant ON customer_identity_audit
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, DELETE ON customer_identity_audit TO madar_app;
