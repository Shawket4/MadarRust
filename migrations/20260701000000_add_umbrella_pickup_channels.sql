-- ═══════════════════════════════════════════════════════════════════
-- Two new ordering channels: 'umbrella' (deliver to a beach/pool umbrella
-- or sunbed by number) and 'pickup' (customer self-collects). Toggleable
-- per branch like in_mall/outside.
--
-- Fully additive: new enum values + new defaulted columns on
-- branch_delivery_settings. Existing channels, orders, and data are
-- untouched. The new enum values are not USED in this migration, so it is
-- transaction-safe on PostgreSQL 12+.
-- ═══════════════════════════════════════════════════════════════════

ALTER TYPE public.delivery_channel ADD VALUE IF NOT EXISTS 'umbrella';
ALTER TYPE public.delivery_channel ADD VALUE IF NOT EXISTS 'pickup';

ALTER TABLE branch_delivery_settings
    ADD COLUMN IF NOT EXISTS umbrella_enabled    boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS pickup_enabled      boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS umbrella_override   text    NOT NULL DEFAULT 'auto',
    ADD COLUMN IF NOT EXISTS pickup_override     text    NOT NULL DEFAULT 'auto',
    ADD COLUMN IF NOT EXISTS umbrella_open_time  time,
    ADD COLUMN IF NOT EXISTS umbrella_close_time time,
    ADD COLUMN IF NOT EXISTS pickup_open_time    time,
    ADD COLUMN IF NOT EXISTS pickup_close_time   time,
    -- Flat per-branch fee (piastres). Umbrella is typically charged; pickup
    -- defaults to free but is configurable.
    ADD COLUMN IF NOT EXISTS umbrella_fee integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS pickup_fee   integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS umbrella_discount_id uuid REFERENCES discounts(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS pickup_discount_id   uuid REFERENCES discounts(id) ON DELETE SET NULL;
