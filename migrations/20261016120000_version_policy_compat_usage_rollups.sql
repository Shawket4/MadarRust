-- DB architecture Phase 1.6 (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.2, §7.3, §8).
-- New tables only. Nothing reads the rollups yet (Phase 2 switches reports
-- after a per-org per-day reconciliation).

-- ── client_version_policy: the minimum-client gate (§7.2) ───────────────────
-- EMPTY ON PURPOSE (owner, 2026-10-16: never force till updates). With no row
-- the gate (src/version_gate.rs) does nothing for any client version, known or
-- not. Only the owner inserts a row, by hand. One row per (app, platform,
-- org): platform NULL = every platform, org_id NULL = every org; the most
-- specific matching row wins. Global platform config, so not tenant-owned:
-- readable by everyone, written only by the owner role.
CREATE TABLE IF NOT EXISTS client_version_policy (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    app              text        NOT NULL CHECK (app IN ('pos', 'kds')),
    platform         text        NULL,
    org_id           uuid        NULL REFERENCES organizations (id) ON DELETE CASCADE,
    -- Below this: still accepted for every write a till may hold offline
    -- (/sync/replay and the ledger routes); 426 on /sync/pull and catalog reads.
    min_supported    text        NULL CHECK (min_supported   ~ '^[0-9]+(\.[0-9]+){0,2}$'),
    -- Below this: answered normally plus `X-Madar-Upgrade: recommended`.
    min_recommended  text        NULL CHECK (min_recommended ~ '^[0-9]+(\.[0-9]+){0,2}$'),
    sunset_at        timestamptz NULL,
    download_url     text        NULL,
    message          jsonb       NOT NULL DEFAULT '{}'::jsonb,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_client_version_policy_scope
    ON client_version_policy (app, COALESCE(platform, ''),
                              COALESCE(org_id, '00000000-0000-0000-0000-000000000000'::uuid));
ALTER TABLE client_version_policy ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS global_read ON client_version_policy;
CREATE POLICY global_read ON client_version_policy FOR SELECT USING (true);
GRANT SELECT ON client_version_policy TO madar_app;

-- ── compat_usage: one counter per compat shim per day per org (§7.3) ────────
-- Written by the server from the owner pool (src/compat_usage.rs, batched
-- once a minute). `org_id` is the nil uuid when the hit had no org.
CREATE TABLE IF NOT EXISTS compat_usage (
    kind     text   NOT NULL,
    site     text   NOT NULL DEFAULT '',
    day      date   NOT NULL,
    org_id   uuid   NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'::uuid,
    hits     bigint NOT NULL DEFAULT 0 CHECK (hits >= 0),
    first_at timestamptz NOT NULL DEFAULT now(),
    last_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, site, day, org_id)
);
CREATE INDEX IF NOT EXISTS idx_compat_usage_day ON compat_usage (day);
ALTER TABLE compat_usage ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON compat_usage;
CREATE POLICY tenant_isolation ON compat_usage FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT ON compat_usage TO madar_app;

-- ── sales_daily_*: per-(branch, business day) rollups (§6.3) ────────────────
-- The business day is the order's local date at its branch
-- (`effective_timezone`), the same rule every report uses today. Voided
-- orders are counted, never summed. Refunds land on their own day.
CREATE TABLE IF NOT EXISTS sales_daily_branch (
    org_id                uuid    NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id             uuid    NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date         date    NOT NULL,
    orders_count          integer NOT NULL,
    voided_count          integer NOT NULL,
    subtotal              bigint  NOT NULL,
    discount_amount       bigint  NOT NULL,
    tax_amount            bigint  NOT NULL,
    service_charge_amount bigint  NOT NULL,
    delivery_fee          bigint  NOT NULL,
    tip_amount            bigint  NOT NULL,
    total_amount          bigint  NOT NULL,
    covers                bigint  NOT NULL,
    refunds_count         integer NOT NULL,
    refund_amount         bigint  NOT NULL,
    refund_tax_amount     bigint  NOT NULL,
    computed_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, branch_id, business_date)
);
CREATE TABLE IF NOT EXISTS sales_daily_item (
    org_id        uuid    NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid    NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date    NOT NULL,
    -- nil uuid for a line with no menu item (an open-price line)
    menu_item_id  uuid    NOT NULL,
    size_label    text    NOT NULL DEFAULT '',
    item_name     text    NOT NULL,
    lines_count   integer NOT NULL,
    quantity      bigint  NOT NULL,
    line_total    bigint  NOT NULL,
    line_cost     bigint  NULL,
    PRIMARY KEY (org_id, branch_id, business_date, menu_item_id, size_label)
);
CREATE TABLE IF NOT EXISTS sales_daily_modifier (
    org_id        uuid    NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid    NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date    NOT NULL,
    -- nil uuid for an add-on line with no id
    addon_item_id uuid    NOT NULL,
    addon_name    text    NOT NULL,
    lines_count   integer NOT NULL,
    quantity      bigint  NOT NULL,
    line_total    bigint  NOT NULL,
    line_cost     bigint  NULL,
    PRIMARY KEY (org_id, branch_id, business_date, addon_item_id)
);
CREATE TABLE IF NOT EXISTS sales_daily_payment (
    org_id         uuid    NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id      uuid    NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date  date    NOT NULL,
    method         text    NOT NULL,
    is_cash        boolean NOT NULL,
    payments_count integer NOT NULL,
    amount         bigint  NOT NULL,
    PRIMARY KEY (org_id, branch_id, business_date, method, is_cash)
);
CREATE INDEX IF NOT EXISTS idx_sales_daily_branch_branch ON sales_daily_branch (branch_id, business_date);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sales_daily_branch', 'sales_daily_item',
                             'sales_daily_modifier', 'sales_daily_payment'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO madar_app', t);
    END LOOP;
END $$;

-- Recompute one (branch, business day) from the ledger: DELETE + INSERT in the
-- caller's transaction, so rerunning it gives the same rows (idempotent).
-- Runs as the caller: under RLS it can only see and write the caller's org.
CREATE OR REPLACE FUNCTION sales_daily_recompute(p_branch uuid, p_date date)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    v_org  uuid;
    v_tz   text;
    v_from timestamptz;
    v_to   timestamptz;
BEGIN
    SELECT org_id INTO v_org FROM branches WHERE id = p_branch;
    IF v_org IS NULL THEN
        RETURN;
    END IF;
    v_tz   := effective_timezone(p_branch);
    v_from := p_date::timestamp AT TIME ZONE v_tz;
    v_to   := (p_date + 1)::timestamp AT TIME ZONE v_tz;

    DELETE FROM sales_daily_branch   WHERE org_id = v_org AND branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_item     WHERE org_id = v_org AND branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_modifier WHERE org_id = v_org AND branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_payment  WHERE org_id = v_org AND branch_id = p_branch AND business_date = p_date;

    INSERT INTO sales_daily_branch
        (org_id, branch_id, business_date, orders_count, voided_count, subtotal,
         discount_amount, tax_amount, service_charge_amount, delivery_fee, tip_amount,
         total_amount, covers, refunds_count, refund_amount, refund_tax_amount)
    SELECT v_org, p_branch, p_date,
           count(*) FILTER (WHERE o.status <> 'voided'),
           count(*) FILTER (WHERE o.status =  'voided'),
           COALESCE(sum(o.subtotal)              FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.discount_amount)       FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.tax_amount)            FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.service_charge_amount) FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.delivery_fee)          FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.tip_amount)            FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.total_amount)          FILTER (WHERE o.status <> 'voided'), 0),
           COALESCE(sum(o.covers)                FILTER (WHERE o.status <> 'voided'), 0),
           r.n, r.amount, r.tax
      FROM (SELECT count(*)::int AS n, COALESCE(sum(amount), 0) AS amount,
                   COALESCE(sum(tax_amount), 0) AS tax
              FROM order_refunds
             WHERE branch_id = p_branch AND issued_at >= v_from AND issued_at < v_to) r
      LEFT JOIN orders o
        ON o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
     GROUP BY r.n, r.amount, r.tax
    HAVING count(o.id) > 0 OR r.n > 0;

    INSERT INTO sales_daily_item
        (org_id, branch_id, business_date, menu_item_id, size_label, item_name,
         lines_count, quantity, line_total, line_cost)
    SELECT v_org, p_branch, p_date,
           COALESCE(oi.menu_item_id, '00000000-0000-0000-0000-000000000000'::uuid),
           COALESCE(oi.size_label, ''), max(oi.item_name),
           count(*), sum(oi.quantity), sum(oi.line_total), sum(oi.line_cost)
      FROM orders o JOIN order_items oi ON oi.order_id = o.id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status <> 'voided'
     GROUP BY 4, 5;

    INSERT INTO sales_daily_modifier
        (org_id, branch_id, business_date, addon_item_id, addon_name,
         lines_count, quantity, line_total, line_cost)
    SELECT v_org, p_branch, p_date,
           COALESCE(a.addon_item_id, '00000000-0000-0000-0000-000000000000'::uuid),
           max(a.addon_name), count(*), sum(a.quantity), sum(a.line_total), sum(a.line_cost)
      FROM orders o
      JOIN order_items oi ON oi.order_id = o.id
      JOIN order_item_addons a ON a.order_item_id = oi.id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status <> 'voided'
     GROUP BY 4;

    INSERT INTO sales_daily_payment
        (org_id, branch_id, business_date, method, is_cash, payments_count, amount)
    SELECT v_org, p_branch, p_date, p.method, p.is_cash, count(*), sum(p.amount)
      FROM orders o JOIN order_payments p ON p.order_id = o.id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status <> 'voided'
     GROUP BY p.method, p.is_cash;
END
$fn$;

-- Every business day a till's sales (and refunds) fell on, recomputed. Called
-- after a till close.
CREATE OR REPLACE FUNCTION sales_daily_recompute_till(p_till uuid)
RETURNS integer LANGUAGE plpgsql AS $fn$
DECLARE
    v_branch uuid;
    v_day    date;
    n        integer := 0;
BEGIN
    SELECT branch_id INTO v_branch FROM tills WHERE id = p_till;
    IF v_branch IS NULL THEN
        RETURN 0;
    END IF;
    FOR v_day IN
        SELECT DISTINCT (created_at AT TIME ZONE effective_timezone(v_branch))::date
          FROM orders WHERE till_id = p_till
        UNION
        SELECT DISTINCT (issued_at AT TIME ZONE effective_timezone(v_branch))::date
          FROM order_refunds WHERE till_id = p_till
    LOOP
        PERFORM sales_daily_recompute(v_branch, v_day);
        n := n + 1;
    END LOOP;
    RETURN n;
END
$fn$;
GRANT EXECUTE ON FUNCTION sales_daily_recompute(uuid, date), sales_daily_recompute_till(uuid) TO madar_app;
