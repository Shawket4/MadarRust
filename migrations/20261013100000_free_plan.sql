-- The Free plan (FREEMIUM_SPEC_2026-10-11.md §4).
--
-- Every organization that exists today stays `paid`: the column's default is
-- the plan of every row already here, so nothing changes for anyone until the
-- owner (super admin) switches a plan, or a shop signs itself up.
--
-- What a plan allows is defined once, in madar-shared (`madar-plan`); the
-- server stores only WHICH plan an organization is on.

ALTER TABLE organizations
    ADD COLUMN plan                      text        NOT NULL DEFAULT 'paid',
    -- 'admin': the super admin created it. 'self': the shop signed itself up
    -- (`POST /public/signup`).
    ADD COLUMN signup_source             text        NOT NULL DEFAULT 'admin',
    -- The terms the owner accepted when signing up, and when. NULL for an org
    -- the super admin made.
    ADD COLUMN signup_terms_version      text        NULL,
    ADD COLUMN signup_terms_accepted_at  timestamptz NULL,
    ADD CONSTRAINT organizations_plan_is_known CHECK (plan IN ('free', 'paid')),
    ADD CONSTRAINT organizations_signup_source_is_known CHECK (signup_source IN ('admin', 'self'));

CREATE INDEX organizations_plan ON organizations (plan) WHERE deleted_at IS NULL;

COMMENT ON COLUMN organizations.plan IS
    'free | paid. The capability ceiling, the limits and the public surfaces follow it (madar-plan). Unknown values are read as paid by the crate, so an old server never locks a paying shop out.';

-- A plan change moves every permission in the org at once, so it bumps the
-- org's authz epoch: the server's per-person cache (keyed by epoch) reloads,
-- and each till's signed snapshot is stale and refreshed. A TRIGGER, not a
-- call in the handler, so a plan changed by any route (or by hand) cannot
-- leave a stale ceiling behind.
CREATE FUNCTION organizations_plan_bumps_epoch() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    PERFORM authz_bump_epoch(NEW.id);
    RETURN NULL;
END $$;

CREATE TRIGGER organizations_plan_changed
    AFTER UPDATE OF plan ON organizations
    FOR EACH ROW WHEN (OLD.plan IS DISTINCT FROM NEW.plan)
    EXECUTE FUNCTION organizations_plan_bumps_epoch();

-- The plan rides the sync feed (branch settings), so a till learns it dropped to
-- Free at its next pull. Same function as 20260917090000, plus `plan`.
CREATE OR REPLACE FUNCTION sync_emit_organizations() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF (OLD.tax_rate, OLD.tax_inclusive, OLD.service_charge_rate, OLD.service_charge_taxable, OLD.require_table_for_orders, OLD.plan)
       IS DISTINCT FROM
       (NEW.tax_rate, NEW.tax_inclusive, NEW.service_charge_rate, NEW.service_charge_taxable, NEW.require_table_for_orders, NEW.plan) THEN
        PERFORM sync_touch_org_branch_settings(NEW.id, NULL);
    END IF;
    RETURN NULL;
END $$;

-- A downgraded organization (paid -> Free) keeps all its branches and sells
-- with ONE till per branch: the most recently used stays active, the owner may
-- pick another (`PUT /orgs/{id}/plan/active-till`). Every other till is
-- `plan_inactive`: it shows "This till isn't active on the Free plan" instead
-- of the sell screen, and a NEW sale or till session from it is refused
-- (`plan_till_inactive`). Upgrading clears the flag.
ALTER TABLE devices
    ADD COLUMN plan_inactive          boolean     NOT NULL DEFAULT false,
    -- When the flag was set: a queued (offline) sale stamped before it is
    -- history, and is always accepted.
    ADD COLUMN plan_inactive_at       timestamptz NULL,
    -- When the till was first TOLD (its first pull after the flag). A queued
    -- sale stamped after that is from a till that knew; before it, the till
    -- could not have known, so the money it took is accepted and flagged.
    ADD COLUMN plan_inactive_seen_at  timestamptz NULL,
    ADD CONSTRAINT devices_plan_inactive_stamped
        CHECK (plan_inactive OR (plan_inactive_at IS NULL AND plan_inactive_seen_at IS NULL));

COMMENT ON COLUMN devices.plan_inactive IS
    'A till the Free plan switched off (not its branch''s active till). Only POS devices.';

-- The WhatsApp OTP store also proves the phone of a shop signing up
-- (`POST /public/signup/otp`). A code belongs to one PURPOSE, so a signup code
-- can never be spent as a customer's device proof, nor the other way round.
ALTER TABLE delivery_otp
    ADD COLUMN purpose text NOT NULL DEFAULT 'delivery'
        CONSTRAINT delivery_otp_purpose_known CHECK (purpose IN ('delivery', 'signup'));
CREATE INDEX delivery_otp_signup_phone ON delivery_otp (phone, created_at) WHERE purpose = 'signup';
