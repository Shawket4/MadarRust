-- Online ordering & delivery — core data model.
-- New channels (in-mall / outside delivery), per-branch delivery settings,
-- OSRM road-distance zone rings, the standalone delivery_orders entity (lives
-- across shifts; materialized into a normal orders row only at finalize), the
-- WhatsApp OTP store, and a delivery_ref counter mirroring order_ref_counters.
-- Money is integer piastres throughout. New ENUMs are created and used in this
-- same file (CREATE TYPE is transaction-safe; only ALTER TYPE ... ADD VALUE is not).

CREATE TYPE public.delivery_channel AS ENUM ('in_mall', 'outside');

CREATE TYPE public.delivery_order_status AS ENUM (
    'received',
    'confirmed',
    'preparing',
    'ready',
    'out_for_delivery',
    'delivered',
    'cancelled',
    'rejected'
);

-- Per-branch delivery config. *_enabled = hard master switch (dashboard /
-- delivery_settings permission); if off, the channel is closed and the POS
-- cannot override it. *_override = the POS toggle (delivery_orders permission):
-- 'auto' follows the daily window, 'open' force-accepts now (ignores the window),
-- 'closed' pauses. The daily window is per channel in the branch timezone; NULL
-- times = no restriction; a close < open window spans midnight. The in-mall fee is
-- a flat per-branch amount; the outside fee comes entirely from the matched zone
-- ring. Effective-open is derived live (no cron):
--   enabled AND open-shift AND override <> 'closed'
--           AND (override = 'open' OR within the daily window)
CREATE TABLE public.branch_delivery_settings (
    branch_id                  uuid PRIMARY KEY REFERENCES public.branches(id) ON DELETE CASCADE,
    in_mall_enabled            boolean      NOT NULL DEFAULT false,
    outside_enabled            boolean      NOT NULL DEFAULT false,
    in_mall_override           text         NOT NULL DEFAULT 'auto',
    outside_override           text         NOT NULL DEFAULT 'auto',
    in_mall_open_time          time,
    in_mall_close_time         time,
    outside_open_time          time,
    outside_close_time         time,
    in_mall_fee                integer      NOT NULL DEFAULT 0,
    prep_time_minutes          integer      NOT NULL DEFAULT 20,
    max_road_distance_meters   integer,
    updated_at                 timestamptz  NOT NULL DEFAULT now(),
    CONSTRAINT bds_in_mall_override_chk    CHECK (in_mall_override IN ('auto', 'open', 'closed')),
    CONSTRAINT bds_outside_override_chk    CHECK (outside_override IN ('auto', 'open', 'closed')),
    CONSTRAINT bds_in_mall_fee_nonneg      CHECK (in_mall_fee >= 0),
    CONSTRAINT bds_prep_nonneg             CHECK (prep_time_minutes >= 0),
    CONSTRAINT bds_max_dist_pos            CHECK (max_road_distance_meters IS NULL OR max_road_distance_meters > 0)
);

-- Concentric outside-delivery rings, matched by OSRM road distance. The smallest
-- active ring whose max_road_distance_meters >= the road distance wins; that ring's
-- `fee` (a flat amount configured per zone in the dashboard) is the delivery fee.
CREATE TABLE public.delivery_zones (
    id                       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    branch_id                uuid        NOT NULL REFERENCES public.branches(id) ON DELETE CASCADE,
    name                     text        NOT NULL,
    name_translations        jsonb       NOT NULL DEFAULT '{}'::jsonb,
    max_road_distance_meters integer     NOT NULL,
    fee                      integer     NOT NULL,
    is_active                boolean     NOT NULL DEFAULT true,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),
    -- Rings are ordered/matched purely by distance — the smallest covering ring
    -- wins. One ring per distance per branch keeps them unambiguous.
    CONSTRAINT delivery_zones_dist_unique UNIQUE (branch_id, max_road_distance_meters),
    CONSTRAINT delivery_zones_dist_pos    CHECK (max_road_distance_meters > 0),
    CONSTRAINT delivery_zones_fee_nonneg  CHECK (fee >= 0)
);
CREATE INDEX idx_delivery_zones_branch ON public.delivery_zones (branch_id);

-- The standalone delivery order. Holds ONLY the order info + frozen snapshots;
-- it is NOT tied to a shift and survives across shift open/closes. `cart` is the
-- frozen priced line snapshot (drives billing + COGS at finalize); the separate
-- `deductions_snapshot` is the frozen ingredient deduction plan (drives inventory
-- at finalize, or waste on cancel when cancel_restocked = false). A real orders
-- row is created only at finalize and linked via order_id. delivery_ref is minted
-- at intake (D-<branchcode>-<YYMMDD>-<NNNN>); the orders row gets its own order_ref.
CREATE TABLE public.delivery_orders (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id               uuid NOT NULL REFERENCES public.organizations(id) ON DELETE CASCADE,
    branch_id            uuid NOT NULL REFERENCES public.branches(id)      ON DELETE CASCADE,
    channel              public.delivery_channel      NOT NULL,
    status               public.delivery_order_status NOT NULL DEFAULT 'received',
    delivery_ref         text,
    -- customer + location (structured fields serve both in-mall and outside)
    customer_name        text NOT NULL,
    customer_phone       text NOT NULL,
    place_name           text,
    floor                text,
    unit_number          text,
    landmark             text,
    address_line         text,
    delivery_notes       text,
    customer_lat         double precision,
    customer_lng         double precision,
    delivery_zone_id     uuid REFERENCES public.delivery_zones(id) ON DELETE SET NULL,
    road_distance_meters integer,
    -- money (integer piastres)
    subtotal             integer NOT NULL DEFAULT 0,
    delivery_fee         integer NOT NULL DEFAULT 0,
    total                integer NOT NULL DEFAULT 0,
    -- frozen snapshots: the single source of truth at finalize
    cart                 jsonb NOT NULL,
    deductions_snapshot  jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- payment hint (teller confirms / overrides the real method at finalize)
    payment_method_hint  text,
    -- base prep time lives on branch_delivery_settings; the teller can add to it
    -- per order in 5-minute increments.
    extra_prep_minutes   integer NOT NULL DEFAULT 0,
    -- phone verification
    otp_verified         boolean NOT NULL DEFAULT false,
    -- the materialized sale (set at finalize)
    order_id             uuid REFERENCES public.orders(id) ON DELETE SET NULL,
    -- customer receipt prints once at confirm/accept (never reprinted at finalize)
    receipt_printed_at   timestamptz,
    -- lifecycle timestamps (created_at == received)
    confirmed_at         timestamptz,
    preparing_at         timestamptz,
    ready_at             timestamptz,
    out_for_delivery_at  timestamptz,
    delivered_at         timestamptz,
    cancelled_at         timestamptz,
    rejected_at          timestamptz,
    cancel_reason        text,
    cancelled_by         uuid,
    -- on cancel: did stock stay available (true) or was the food made & wasted (false)?
    cancel_restocked     boolean,
    idempotency_key      uuid,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT delivery_orders_money_nonneg CHECK (subtotal >= 0 AND delivery_fee >= 0 AND total >= 0 AND extra_prep_minutes >= 0)
);
CREATE INDEX        idx_delivery_orders_branch_status ON public.delivery_orders (branch_id, status);
CREATE INDEX        idx_delivery_orders_org           ON public.delivery_orders (org_id);
CREATE INDEX        idx_delivery_orders_phone         ON public.delivery_orders (customer_phone);
CREATE INDEX        idx_delivery_orders_order         ON public.delivery_orders (order_id)        WHERE order_id IS NOT NULL;
CREATE UNIQUE INDEX uq_delivery_orders_idem           ON public.delivery_orders (idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX uq_delivery_orders_ref            ON public.delivery_orders (delivery_ref)    WHERE delivery_ref IS NOT NULL;

-- WhatsApp OTP store (send-only gateway; the code is typed back into the web form).
CREATE TABLE public.delivery_otp (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    phone       text        NOT NULL,
    code_hash   text        NOT NULL,
    attempts    integer     NOT NULL DEFAULT 0,
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_delivery_otp_phone   ON public.delivery_otp (phone);
CREATE INDEX idx_delivery_otp_expires ON public.delivery_otp (expires_at);

-- Per-(branch, business_date) sequence for delivery_ref — mirrors
-- order_ref_counters but kept separate so delivery refs get their own clean run,
-- minted at intake (the materialized order still mints a normal order_ref).
CREATE TABLE public.delivery_ref_counters (
    branch_id     uuid    NOT NULL REFERENCES public.branches(id),
    business_date date    NOT NULL,
    last_seq      integer NOT NULL DEFAULT 0,
    PRIMARY KEY (branch_id, business_date)
);
