-- DB architecture Phase 3.3 (DB_ARCHITECTURE_AUDIT_2026-10-16 §7.3, §8 Phase 3.3):
-- the `compat` schema.
--
-- Every view that exists only to serve an OLD client shape gets its home here:
-- the 12 legacy catalog views and loyalty_members_v. Only `src/compat`'s
-- registry and the old-shape handlers it lists name `compat.*`
-- (tests/compat_contract_tests.rs fails on any other reference), always
-- schema-qualified, so no search_path trick is involved.
--
-- The definitions are copied from the public views at apply time. Since Phase
-- 2.4 those are prod's exact text on every database, so compat.X == public.X.
-- One deliberate difference: compat.menu_item_allowed_addons reads the
-- attachment option table (Phase 3.4) instead of the array, with the same
-- rows and order.
--
-- The PUBLIC views stay for one release: a rolling deploy's previous container
-- still reads them while the new one boots. Dropping them is parked
-- (migrations/parked/p3_drop_public_compat_views.sql). Views hold no data, so
-- neither step can lose any.
SET LOCAL lock_timeout = '15s';

CREATE SCHEMA IF NOT EXISTS compat;
REVOKE ALL ON SCHEMA compat FROM PUBLIC;
GRANT USAGE ON SCHEMA compat TO madar_app;

DO $compat$
DECLARE
    v text;
BEGIN
    FOREACH v IN ARRAY ARRAY[
        'addon_item_ingredients', 'addon_items', 'branch_addon_overrides',
        'branch_channel_addon_overrides', 'branch_channel_menu_overrides',
        'branch_menu_overrides', 'branch_menu_size_overrides', 'item_sizes',
        'menu_item_addon_slots', 'menu_item_optional_fields', 'menu_item_recipes',
        'loyalty_members_v'] LOOP
        IF (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.' || v)) IS DISTINCT FROM 'v' THEN
            RAISE EXCEPTION 'p3 compat: public.% is not a view (run 20261018200000 first)', v;
        END IF;
        EXECUTE format('CREATE OR REPLACE VIEW compat.%I WITH (security_invoker = true) AS %s',
                       v, pg_get_viewdef(format('public.%I', v)::regclass));
        EXECUTE format('GRANT SELECT ON compat.%I TO madar_app', v);
    END LOOP;
END
$compat$;

-- The allowlist, from the attachment option table: an attachment WITH an
-- allowlist lists its add-on options in allowlist order; one without lists
-- nothing ("no rows = the org's catalog", as before).
CREATE OR REPLACE VIEW compat.menu_item_allowed_addons WITH (security_invoker = true) AS
SELECT mimg.menu_item_id, x.option_id AS addon_item_id, x.sort AS sort_order, now() AS created_at
  FROM menu_item_modifier_groups mimg
  JOIN menu_item_modifier_group_options x ON x.mimg_id = mimg.id
  JOIN modifier_options mo ON mo.id = x.option_id AND mo.legacy_source = 'addon'
 WHERE mimg.option_allowlist;
GRANT SELECT ON compat.menu_item_allowed_addons TO madar_app;
