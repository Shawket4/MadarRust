-- POS discounts (phase 6): what kind of discount a sale carried, who applied
-- it and which manager approval let it over the person's cap.
--
-- `discount_type` / `discount_value` / `discount_amount` / `discount_id`
-- already record the money. These columns record the ACT:
--   discount_kind         preset | manual_amount | manual_percent
--                         (the capability that was exercised)
--   discount_percent_bps  the percentage asked for, in basis points, when the
--                         discount is a percentage (1250 = 12.5%)
--   discount_applied_by   the person who put it on the sale
--   discount_approval_id  the manager approval (`approvals.id`) that let it
--                         past the person's cap, when one did
-- All nullable and additive: older rows and older tills leave them empty.
ALTER TABLE orders
    ADD COLUMN IF NOT EXISTS discount_kind text NULL
        CHECK (discount_kind IN ('preset', 'manual_amount', 'manual_percent')),
    ADD COLUMN IF NOT EXISTS discount_percent_bps integer NULL
        CHECK (discount_percent_bps BETWEEN 0 AND 10000),
    ADD COLUMN IF NOT EXISTS discount_applied_by uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS discount_approval_id uuid NULL;

-- A discount approval is asked for a percentage as well as an amount.
ALTER TABLE approvals ADD COLUMN IF NOT EXISTS percent_bps bigint NULL;

-- Which row a flag is about (the order a flagged CreateOrder wrote), so an
-- audit can show the flag beside the sale. Nullable: older flags name none.
ALTER TABLE authz_replay_flags ADD COLUMN IF NOT EXISTS subject_id uuid NULL;
CREATE INDEX IF NOT EXISTS authz_replay_flags_subject
    ON authz_replay_flags (subject_id) WHERE subject_id IS NOT NULL;
