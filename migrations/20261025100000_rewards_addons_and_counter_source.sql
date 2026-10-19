-- Rewards by Madar: loyalty sold on its own (Madar REWARDS_DESIGN.md).
--
-- 1. Add-ons: a per-org set of features switched on beside the plan. What an
--    org may use is its plan's features plus its ACTIVE add-ons
--    (madar_plan::entitlements_with). Loyalty-only = plan 'free' + add-on
--    'loyalty'. No billing columns: like the plan, an add-on is switched by the
--    super admin by hand.
-- 2. The ledger's `counter` source: a stamp card stamped, or completed, at the
--    counter with no order (the Rewards by Madar teller page). Additive: old rows and old
--    readers are untouched (`source` reaches every client as a plain string).

CREATE TABLE IF NOT EXISTS org_addons (
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- A madar-plan feature key. Only loyalty is sold as an add-on today.
    feature    text NOT NULL CHECK (feature IN ('loyalty')),
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'ended')),
    started_at timestamptz NOT NULL DEFAULT now(),
    -- Set when it ends: by the super admin, or by the org moving to a plan that
    -- already includes the feature. A later downgrade does NOT restore it.
    ends_at    timestamptz NULL,
    -- Who switched it (audit only, no FK: a super admin's session may have no
    -- users row of its own, and that must never block a grant).
    granted_by uuid NULL,
    note       text NULL,
    PRIMARY KEY (org_id, feature),
    CONSTRAINT org_addons_ended_has_end CHECK ((status = 'ended') = (ends_at IS NOT NULL))
);

ALTER TABLE org_addons ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON org_addons;
CREATE POLICY tenant_isolation ON org_addons FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE ON org_addons TO madar_app;

-- The org's active add-on keys. SECURITY DEFINER so the plan checks read it the
-- same way from any pool (owner, tenant under RLS, public resolver) and from
-- the jobs' SQL (`'loyalty' = ANY(org_active_addons(o.id))`).
CREATE OR REPLACE FUNCTION org_active_addons(p_org uuid) RETURNS text[]
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public AS $$
    SELECT COALESCE(array_agg(feature ORDER BY feature), '{}')
      FROM org_addons WHERE org_id = p_org AND status = 'active'
$$;
GRANT EXECUTE ON FUNCTION org_active_addons(uuid) TO madar_app;

-- An add-on moves permissions like a plan change does: bump the org's authz
-- epoch so cached sets and tills' signed snapshots refresh. A trigger, so any
-- route (or a hand edit) is covered.
CREATE OR REPLACE FUNCTION org_addons_bump_epoch() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    PERFORM authz_bump_epoch(COALESCE(NEW.org_id, OLD.org_id));
    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS org_addons_changed ON org_addons;
CREATE TRIGGER org_addons_changed
    AFTER INSERT OR UPDATE OR DELETE ON org_addons
    FOR EACH ROW EXECUTE FUNCTION org_addons_bump_epoch();

-- Upgrade to paid ends every add-on: paid already includes the feature (owner,
-- 2026-10-19). The row is kept (ended), never deleted. A trigger on the plan
-- column, beside organizations_plan_changed, so a plan changed by any route is
-- covered.
CREATE OR REPLACE FUNCTION organizations_plan_ends_addons() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NEW.plan = 'paid' THEN
        UPDATE org_addons SET status = 'ended', ends_at = now()
         WHERE org_id = NEW.id AND status = 'active';
    END IF;
    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS organizations_plan_ends_addons ON organizations;
CREATE TRIGGER organizations_plan_ends_addons
    AFTER UPDATE OF plan ON organizations
    FOR EACH ROW WHEN (OLD.plan IS DISTINCT FROM NEW.plan)
    EXECUTE FUNCTION organizations_plan_ends_addons();

-- ── The ledger ────────────────────────────────────────────────────────────
-- `counter`: an earn or a redeem made at the counter with no order. Same
-- pairing rules otherwise (20260925050000).
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT IF EXISTS loyalty_txn_source,
    DROP CONSTRAINT IF EXISTS loyalty_txn_kind_has_a_source;
ALTER TABLE loyalty_transactions
    ADD CONSTRAINT loyalty_txn_source CHECK (source IN (
        'sale', 'redemption', 'void', 'refund', 'birthday', 'winback', 'manual',
        'merge',
        'counter'      -- stamped or completed at the counter, no order (Rewards by Madar)
    )),
    ADD CONSTRAINT loyalty_txn_kind_has_a_source CHECK (
        CASE kind
            WHEN 'earn'           THEN source IN ('sale', 'counter')
            WHEN 'redeem'         THEN source IN ('redemption', 'counter')
            WHEN 'adjust'         THEN source IN ('manual', 'birthday', 'winback', 'merge')
            WHEN 'reverse_earn'   THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_redeem' THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_adjust' THEN source = 'manual'
        END
    ),
    -- A counter row names no order.
    ADD CONSTRAINT loyalty_txn_counter_has_no_order CHECK (
        source <> 'counter' OR order_id IS NULL
    );

-- The key the teller page makes per attempt: a retry finds the first row.
ALTER TABLE loyalty_transactions ADD COLUMN IF NOT EXISTS idempotency_key text NULL;
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_transactions_org_idempotency_key
    ON loyalty_transactions (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
