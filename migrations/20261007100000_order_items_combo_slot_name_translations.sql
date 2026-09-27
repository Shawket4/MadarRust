-- Arabic names for combo slots on SOLD lines (owner, 2026-09-27; deferred
-- after the combos merge as `order_items.combo_slot_name_translations`).
--
-- A slot's names already live in the catalogue: `combo_slots.name_translations`
-- (20261005100000_combos.sql) is written by the combo editor and read by the
-- POS feed, the combo sheet and the public menus. What a sale stored was only
-- the English snapshot `order_items.combo_slot_name`, so the order sheet (the
-- dashboard's, the till's reprint data) and the combo-mix report showed "Main"
-- and "Drink" in Arabic. This adds the translations beside that snapshot, the
-- same way `order_items.name_translations` sits beside `item_name`.
--
-- `{}` on every line that is not a combo part, and on parts whose slot had no
-- translations at the sale; readers fall back to `combo_slot_name`.
--
-- Additive and idempotent. No new table, so RLS and grants are the table's
-- own. `order_items` already re-emits `order` on the changefeed (`sync_emit`,
-- 20260914090300_sync_changefeed.sql); only the order projection grows
-- (`OrderItem` in src/orders/handlers.rs), so no SOURCE TABLES line.

ALTER TABLE order_items
    ADD COLUMN IF NOT EXISTS combo_slot_name_translations jsonb NOT NULL DEFAULT '{}'::jsonb;

-- Parts sold before this: take the slot's translations when the slot still
-- exists under the name the sale stored (a renamed slot's new Arabic would not
-- be what was sold, so it stays `{}` and the English snapshot shows). Each row
-- touched re-emits its order once, which is how the tills and the dashboard see
-- the Arabic names on past sales.
UPDATE order_items oi
   SET combo_slot_name_translations = cs.name_translations
  FROM combo_slots cs
 WHERE oi.line_kind = 'combo_part'
   AND oi.combo_slot_id = cs.id
   AND oi.combo_slot_name = cs.name
   AND oi.combo_slot_name_translations = '{}'::jsonb
   AND cs.name_translations <> '{}'::jsonb;
