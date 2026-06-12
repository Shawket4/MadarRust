-- Menu Advisor rebuild: drop & recreate all five advisor tables.
--
-- Advisor rows are regenerable analytics output (a new run repopulates
-- them), so a clean drop is safe. The rebuilt layout stores each suggestion
-- as ONE `payload` JSONB column — byte-identical to the wire shape the API
-- returns — plus STORED generated columns mirroring every field the list
-- endpoints filter or sort on. The payload is the single source of truth;
-- the scalars can never drift from it. Requires PostgreSQL >= 12.

DROP TABLE IF EXISTS menu_advisor_decisions;
DROP TABLE IF EXISTS menu_advisor_price_suggestions;
DROP TABLE IF EXISTS menu_advisor_bundle_suggestions;
DROP TABLE IF EXISTS menu_advisor_removal_scenarios;
DROP TABLE IF EXISTS menu_advisor_runs;

-- ── Runs ─────────────────────────────────────────────────────────────

CREATE TABLE menu_advisor_runs (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    branch_id          uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    status             text NOT NULL DEFAULT 'in_progress'
                            CHECK (status IN ('in_progress', 'completed', 'failed')),
    config             jsonb NOT NULL,
    error_message      text,
    items_total        integer NOT NULL DEFAULT 0,
    items_cm_tracked   integer NOT NULL DEFAULT 0,
    items_revenue_only integer NOT NULL DEFAULT 0,
    items_insufficient integer NOT NULL DEFAULT 0,
    window_days        double precision NOT NULL,
    started_at         timestamptz NOT NULL DEFAULT now(),
    completed_at       timestamptz
);

CREATE INDEX menu_advisor_runs_branch_started_idx
    ON menu_advisor_runs (branch_id, started_at DESC);

-- One in-progress run per branch, enforced by the database. create_run maps
-- a violation of this constraint to HTTP 409, closing the TOCTOU race
-- between two concurrent POSTs. The constraint NAME is matched in code.
CREATE UNIQUE INDEX menu_advisor_runs_one_active_per_branch
    ON menu_advisor_runs (branch_id) WHERE status = 'in_progress';

CREATE INDEX menu_advisor_runs_branch_completed_idx
    ON menu_advisor_runs (branch_id, completed_at DESC) WHERE status = 'completed';

-- ── Price suggestions ────────────────────────────────────────────────

CREATE TABLE menu_advisor_price_suggestions (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id      uuid NOT NULL REFERENCES menu_advisor_runs(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id)          ON DELETE CASCADE,
    -- Not part of the payload (the engine doesn't know categories); supplied
    -- at insert time for the category_id list filter.
    category_id uuid REFERENCES categories(id) ON DELETE SET NULL,
    payload     jsonb NOT NULL,  -- serialized dto::PriceSuggestion (flattened wire shape)
    created_at  timestamptz NOT NULL DEFAULT now(),

    menu_item_id        uuid    GENERATED ALWAYS AS ((payload #>> '{key,menu_item_id}')::uuid) STORED NOT NULL,
    size_label          text    GENERATED ALWAYS AS (payload #>> '{key,size_label}') STORED NOT NULL,
    item_name           text    GENERATED ALWAYS AS (payload ->> 'item_name') STORED NOT NULL,
    classification_mode text    GENERATED ALWAYS AS (payload #>> '{classification,mode}') STORED NOT NULL
                                CHECK (classification_mode IN ('cm', 'revenue', 'insufficient')),
    cm_quadrant         text    GENERATED ALWAYS AS (payload #>> '{classification,quadrant}') STORED,
    revenue_class       text    GENERATED ALWAYS AS (payload #>> '{classification,class}') STORED,
    action              text    GENERATED ALWAYS AS (payload ->> 'action') STORED NOT NULL
                                CHECK (action IN ('hold', 'raise_price', 'lower_price', 'bundle',
                                                  'remove', 'reformulate', 'monitor')),
    confidence          text    GENERATED ALWAYS AS (payload ->> 'confidence') STORED NOT NULL
                                CHECK (confidence IN ('low', 'medium', 'high')),
    popularity_share    double precision GENERATED ALWAYS AS ((payload ->> 'popularity_share')::double precision) STORED NOT NULL,
    current_price       bigint  GENERATED ALWAYS AS ((payload ->> 'current_price')::bigint) STORED NOT NULL,
    suggested_price     bigint  GENERATED ALWAYS AS ((payload ->> 'suggested_price')::bigint) STORED,
    suggested_delta_pct double precision GENERATED ALWAYS AS ((payload ->> 'suggested_delta_pct')::double precision) STORED
);

CREATE INDEX menu_advisor_price_suggestions_run_idx
    ON menu_advisor_price_suggestions (run_id);
CREATE INDEX menu_advisor_price_suggestions_latest_kpi_idx
    ON menu_advisor_price_suggestions (branch_id, menu_item_id, size_label);

-- ── Bundle suggestions ───────────────────────────────────────────────

CREATE TABLE menu_advisor_bundle_suggestions (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id             uuid NOT NULL REFERENCES menu_advisor_runs(id) ON DELETE CASCADE,
    branch_id          uuid NOT NULL REFERENCES branches(id)          ON DELETE CASCADE,
    payload            jsonb NOT NULL,  -- serialized dto::BundleSuggestion
    -- bundles are hard-deleted, hence SET NULL rather than soft-delete logic
    promoted_bundle_id uuid REFERENCES bundles(id) ON DELETE SET NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),

    focus_menu_item_id  uuid    GENERATED ALWAYS AS ((payload #>> '{focus_item,menu_item_id}')::uuid) STORED NOT NULL,
    focus_size_label    text    GENERATED ALWAYS AS (payload #>> '{focus_item,size_label}') STORED NOT NULL,
    missing_costs       boolean GENERATED ALWAYS AS ((payload ->> 'missing_costs')::boolean) STORED NOT NULL,
    bundle_cm           bigint  GENERATED ALWAYS AS ((payload ->> 'bundle_cm')::bigint) STORED,
    bundle_discount_pct double precision GENERATED ALWAYS AS ((payload ->> 'bundle_discount_pct')::double precision) STORED NOT NULL
);

CREATE INDEX menu_advisor_bundle_suggestions_run_idx
    ON menu_advisor_bundle_suggestions (run_id);

-- ── Removal scenarios ────────────────────────────────────────────────

CREATE TABLE menu_advisor_removal_scenarios (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id     uuid NOT NULL REFERENCES menu_advisor_runs(id) ON DELETE CASCADE,
    branch_id  uuid NOT NULL REFERENCES branches(id)          ON DELETE CASCADE,
    payload    jsonb NOT NULL,  -- serialized dto::RemovalScenario
    created_at timestamptz NOT NULL DEFAULT now(),

    menu_item_id   uuid GENERATED ALWAYS AS ((payload #>> '{key,menu_item_id}')::uuid) STORED NOT NULL,
    size_label     text GENERATED ALWAYS AS (payload #>> '{key,size_label}') STORED NOT NULL,
    recommendation text GENERATED ALWAYS AS (payload ->> 'recommendation') STORED NOT NULL
                        CHECK (recommendation IN ('remove', 'keep_and_bundle',
                                                  'keep_and_reformulate', 'no_strong_signal')),
    net_cm_change  double precision GENERATED ALWAYS AS ((payload ->> 'net_cm_change')::double precision) STORED NOT NULL
);

CREATE INDEX menu_advisor_removal_scenarios_run_idx
    ON menu_advisor_removal_scenarios (run_id);

-- ── Decisions ────────────────────────────────────────────────────────
-- suggestion_id is polymorphic across the three suggestion tables, so no FK
-- is possible; record_decision validates existence + branch consistency.
-- Decisions deliberately survive run deletion (audit trail); calibration
-- inner-joins suggestions, so dangling decisions are inert.

CREATE TABLE menu_advisor_decisions (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    suggestion_id   uuid NOT NULL,
    suggestion_kind text NOT NULL CHECK (suggestion_kind IN ('price', 'bundle', 'removal')),
    branch_id       uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    decision        text NOT NULL CHECK (decision IN ('accepted', 'rejected', 'ignored')),
    notes           text,
    decided_by      uuid NOT NULL REFERENCES users(id),
    decided_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX menu_advisor_decisions_suggestion_idx
    ON menu_advisor_decisions (suggestion_id, decided_at DESC);
CREATE INDEX menu_advisor_decisions_branch_idx
    ON menu_advisor_decisions (branch_id, decided_at DESC);
