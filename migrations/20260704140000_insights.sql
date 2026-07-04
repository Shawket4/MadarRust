-- Menu profitability insights (replaces the retired menu advisor / engineering).
--
-- Two small durable pieces; everything else (the margin ledger + signals) is
-- computed live per request from order history + recipe costs:
--
--   margin_targets  — the gross-margin % bar that drives "below target" flags.
--                     One org-default row (branch_id NULL) + optional per-branch
--                     overrides; resolution is branch → org → built-in default.
--   menu_decisions  — APPEND-ONLY log of operator responses to signals
--                     (acted / dismissed / snoozed) with the evidence baseline
--                     frozen at decision time, so impact can be measured from
--                     order history afterwards and dismissed signals can be
--                     suppressed (until the evidence materially worsens).

CREATE TABLE margin_targets (
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id  uuid REFERENCES branches(id) ON DELETE CASCADE, -- NULL = org default
    target_pct numeric(5,2) NOT NULL CHECK (target_pct > 0 AND target_pct < 100),
    updated_by uuid REFERENCES users(id) ON DELETE SET NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (org_id, branch_id)
);

CREATE TABLE menu_decisions (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid REFERENCES branches(id) ON DELETE CASCADE, -- NULL = org-wide
    menu_item_id uuid NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    size_label   text NOT NULL DEFAULT 'one_size',
    signal_kind  text NOT NULL CHECK (signal_kind IN
        ('below_cost','below_target','cost_spike','price_candidate',
         'removal_candidate','recipe_incomplete')),
    action       text NOT NULL CHECK (action IN ('acted','dismissed','snoozed')),
    -- Operator-supplied context, e.g. {"old_price":6000,"new_price":6500,"note":"…"}.
    detail       jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- Evidence frozen at decision time (server-computed): {"window_days":28,
    -- "quantity":…, "revenue":…, "cost":…, "margin_pct":…}. Impact = the same
    -- aggregate over the window AFTER created_at, compared to this.
    baseline     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_by   uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Latest-decision-per-signal lookups (suppression) + org log listing.
CREATE INDEX menu_decisions_sku_kind_idx
    ON menu_decisions (org_id, menu_item_id, size_label, signal_kind, created_at DESC);
CREATE INDEX menu_decisions_org_created_idx
    ON menu_decisions (org_id, created_at DESC);

GRANT ALL ON TABLE margin_targets TO sufrix;
GRANT ALL ON TABLE menu_decisions TO sufrix;
