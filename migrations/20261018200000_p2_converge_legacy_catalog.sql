-- DB architecture Phase 2.4 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 2.4):
-- converge the migrations with prod.
--
-- Prod had `deploy/menu_unification_shim.sql` applied by hand: the 12 legacy
-- catalog tables were dropped and 11 of them recreated as read-only views over
-- the unified tables (`menu_item_addon_overrides` got no view). A database
-- built from migrations/ alone still had the TABLES, so tests, CI and the
-- compile-time query checks ran against a schema prod does not have.
--
-- This migration gives every database prod's shape, WITHOUT DROPPING DATA:
--   * a legacy relation that is still a TABLE in `public` is MOVED to the
--     `archive` schema (rows kept as they are, whatever they hold), its FKs to
--     and from live tables dropped (constraints, not data: an archived row must
--     never be cascade-deleted by a live delete, and live rows must not depend
--     on a frozen table), its triggers dropped (it is never written again), and
--     tenant access to it revoked;
--   * then the view prod has is created under the old name, with prod's exact
--     definition (pg_get_viewdef of prod), `security_invoker` (so the tenant's
--     RLS applies through it) and prod's grants.
-- In prod every name is already a view and `menu_item_addon_overrides` is
-- gone, so the whole file is a no-op there.
--
-- `deploy/menu_unification_shim.sql` is deleted in the same change: this
-- migration replaces it. The drift catalog (scripts/schema_catalog.sql) leaves
-- the `archive` schema out: what it holds legitimately differs per database.
SET LOCAL lock_timeout = '15s';

CREATE SCHEMA IF NOT EXISTS archive;
REVOKE ALL ON SCHEMA archive FROM PUBLIC;

DO $p24$
DECLARE
    legacy text[] := ARRAY[
        'menu_item_addon_overrides', 'branch_channel_addon_overrides',
        'branch_channel_menu_overrides', 'branch_addon_overrides',
        'branch_menu_size_overrides', 'branch_menu_overrides',
        'menu_item_allowed_addons', 'menu_item_optional_fields',
        'menu_item_addon_slots', 'addon_item_ingredients', 'addon_items',
        'menu_item_recipes'];
    t   text;
    r   record;
    moved int := 0;
BEGIN
    -- Only ever a TABLE in public is moved; a view (prod) is left alone.
    FOR t IN SELECT unnest(legacy) LOOP
        IF to_regclass('public.' || t) IS NULL
           OR (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.' || t)) <> 'r' THEN
            CONTINUE;
        END IF;
        IF to_regclass('archive.' || t) IS NOT NULL THEN
            RAISE EXCEPTION 'p2.4: archive.% already exists; refusing to overwrite it', t;
        END IF;
        -- FKs that cross between this table and any table (constraints only).
        FOR r IN
            SELECT conrelid::regclass AS rel, conname
              FROM pg_constraint
             WHERE contype = 'f'
               AND (conrelid = to_regclass('public.' || t) OR confrelid = to_regclass('public.' || t))
        LOOP
            EXECUTE format('ALTER TABLE %s DROP CONSTRAINT IF EXISTS %I', r.rel, r.conname);
        END LOOP;
        FOR r IN
            SELECT tgname FROM pg_trigger
             WHERE tgrelid = to_regclass('public.' || t) AND NOT tgisinternal
        LOOP
            EXECUTE format('DROP TRIGGER %I ON public.%I', r.tgname, t);
        END LOOP;
        EXECUTE format('ALTER TABLE public.%I SET SCHEMA archive', t);
        EXECUTE format('REVOKE ALL ON archive.%I FROM PUBLIC', t);
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'madar_app') THEN
            EXECUTE format('REVOKE ALL ON archive.%I FROM madar_app', t);
        END IF;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            EXECUTE format('REVOKE ALL ON archive.%I FROM sufrix', t);
        END IF;
        moved := moved + 1;
    END LOOP;
    IF moved > 0 THEN
        RAISE NOTICE 'p2.4: % legacy catalog tables moved to archive', moved;
    END IF;
END
$p24$;

-- addon_item_ingredients
DO $v$
BEGIN
    IF to_regclass('public.addon_item_ingredients') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.addon_item_ingredients WITH (security_invoker = true) AS
 SELECT rl.id,
    rl.owner_id AS addon_item_id,
    rl.quantity AS quantity_used,
    rl.created_at,
    rl.updated_at,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.ingredient_id AS org_ingredient_id
   FROM ((recipe_lines rl
     JOIN modifier_options mo ON (((mo.id = rl.owner_id) AND (mo.legacy_source = 'addon'::text))))
     JOIN org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE ((rl.owner_type = 'modifier_option'::text) AND (rl.size_label IS NULL))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.addon_item_ingredients TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.addon_item_ingredients TO sufrix;
        END IF;
    END IF;
END
$v$;

-- addon_items
DO $v$
BEGIN
    IF to_regclass('public.addon_items') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.addon_items WITH (security_invoker = true) AS
 SELECT mo.id,
    mg.org_id,
    mo.name,
    mg.legacy_addon_type AS type,
    mo.price AS default_price,
    mo.is_active,
    mo.name_translations,
    mo.created_at,
    mo.updated_at
   FROM (modifier_options mo
     JOIN modifier_groups mg ON ((mg.id = mo.group_id)))
  WHERE (mo.legacy_source = 'addon'::text)$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.addon_items TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.addon_items TO sufrix;
        END IF;
    END IF;
END
$v$;

-- branch_addon_overrides
DO $v$
BEGIN
    IF to_regclass('public.branch_addon_overrides') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.branch_addon_overrides WITH (security_invoker = true) AS
 SELECT p.branch_id,
    p.target_id AS addon_item_id,
    p.price AS price_override,
    COALESCE(p.is_available, true) AS is_available,
    p.updated_at
   FROM (menu_price_overrides p
     JOIN modifier_options mo ON (((mo.id = p.target_id) AND (mo.legacy_source = 'addon'::text))))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'modifier_option'::text))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.branch_addon_overrides TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.branch_addon_overrides TO sufrix;
        END IF;
    END IF;
END
$v$;

-- branch_channel_addon_overrides
DO $v$
BEGIN
    IF to_regclass('public.branch_channel_addon_overrides') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.branch_channel_addon_overrides WITH (security_invoker = true) AS
 SELECT p.branch_id,
    p.target_id AS addon_item_id,
    p.channel,
    p.price AS price_override,
    p.is_available,
    p.updated_at
   FROM (menu_price_overrides p
     JOIN modifier_options mo ON (((mo.id = p.target_id) AND (mo.legacy_source = 'addon'::text))))
  WHERE ((p.scope = 'branch_channel'::text) AND (p.target_type = 'modifier_option'::text))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.branch_channel_addon_overrides TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.branch_channel_addon_overrides TO sufrix;
        END IF;
    END IF;
END
$v$;

-- branch_channel_menu_overrides
DO $v$
BEGIN
    IF to_regclass('public.branch_channel_menu_overrides') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.branch_channel_menu_overrides WITH (security_invoker = true) AS
 SELECT p.branch_id,
    ms.menu_item_id,
    p.channel,
    max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) AS price_override,
    bool_and(p.is_available) AS is_available,
    max(p.updated_at) AS updated_at
   FROM (menu_price_overrides p
     JOIN menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch_channel'::text) AND (p.target_type = 'menu_item_size'::text))
  GROUP BY p.branch_id, ms.menu_item_id, p.channel
 HAVING ((max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) IS NOT NULL) OR bool_or((p.is_available IS NOT NULL)))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.branch_channel_menu_overrides TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.branch_channel_menu_overrides TO sufrix;
        END IF;
    END IF;
END
$v$;

-- branch_menu_overrides
DO $v$
BEGIN
    IF to_regclass('public.branch_menu_overrides') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.branch_menu_overrides WITH (security_invoker = true) AS
 SELECT p.branch_id,
    ms.menu_item_id,
    max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) AS price_override,
    COALESCE(bool_and(p.is_available), true) AS is_available,
    max(p.updated_at) AS updated_at
   FROM (menu_price_overrides p
     JOIN menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'menu_item_size'::text))
  GROUP BY p.branch_id, ms.menu_item_id
 HAVING ((max(p.price) FILTER (WHERE (ms.label = 'one_size'::text)) IS NOT NULL) OR bool_or((p.is_available = false)))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.branch_menu_overrides TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.branch_menu_overrides TO sufrix;
        END IF;
    END IF;
END
$v$;

-- branch_menu_size_overrides
DO $v$
BEGIN
    IF to_regclass('public.branch_menu_size_overrides') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.branch_menu_size_overrides WITH (security_invoker = true) AS
 SELECT p.branch_id,
    ms.menu_item_id,
    ms.label AS size_label,
    p.price AS price_override,
    p.updated_at
   FROM (menu_price_overrides p
     JOIN menu_item_sizes ms ON ((ms.id = p.target_id)))
  WHERE ((p.scope = 'branch'::text) AND (p.target_type = 'menu_item_size'::text) AND (p.price IS NOT NULL) AND (ms.label <> 'one_size'::text))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.branch_menu_size_overrides TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.branch_menu_size_overrides TO sufrix;
        END IF;
    END IF;
END
$v$;

-- menu_item_addon_slots
DO $v$
BEGIN
    IF to_regclass('public.menu_item_addon_slots') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.menu_item_addon_slots WITH (security_invoker = true) AS
 SELECT mimg.id,
    mimg.menu_item_id,
    mg.legacy_addon_type AS addon_type,
    COALESCE(mimg.is_required_override, mg.is_required) AS is_required,
    COALESCE(mimg.min_override, mg.min_selections) AS min_selections,
    COALESCE(mimg.max_override, mg.max_selections) AS max_selections,
    now() AS created_at,
    mg.name AS label,
    mg.name_translations AS label_translations
   FROM (menu_item_modifier_groups mimg
     JOIN modifier_groups mg ON ((mg.id = mimg.group_id)))
  WHERE ((mimg.legacy_origin = 'slot'::text) AND (mg.legacy_addon_type IS NOT NULL))$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.menu_item_addon_slots TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.menu_item_addon_slots TO sufrix;
        END IF;
    END IF;
END
$v$;

-- menu_item_allowed_addons
DO $v$
BEGIN
    IF to_regclass('public.menu_item_allowed_addons') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.menu_item_allowed_addons WITH (security_invoker = true) AS
 SELECT mimg.menu_item_id,
    u.opt AS addon_item_id,
    ((u.ord - 1))::integer AS sort_order,
    now() AS created_at
   FROM ((menu_item_modifier_groups mimg
     CROSS JOIN LATERAL unnest(mimg.included_option_ids) WITH ORDINALITY u(opt, ord))
     JOIN modifier_options mo ON (((mo.id = u.opt) AND (mo.legacy_source = 'addon'::text))))
  WHERE (mimg.included_option_ids IS NOT NULL)$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.menu_item_allowed_addons TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.menu_item_allowed_addons TO sufrix;
        END IF;
    END IF;
END
$v$;

-- menu_item_optional_fields
DO $v$
BEGIN
    IF to_regclass('public.menu_item_optional_fields') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.menu_item_optional_fields WITH (security_invoker = true) AS
 SELECT mo.id,
    mimg.menu_item_id,
    mo.name,
    mo.price,
    rl.ingredient_id AS org_ingredient_id,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.quantity AS quantity_used,
    NULL::text AS size_label,
    mo.is_active,
    mo.created_at,
    mo.updated_at,
    mo.name_translations
   FROM (((modifier_options mo
     JOIN menu_item_modifier_groups mimg ON (((mimg.group_id = mo.group_id) AND (mimg.legacy_origin = 'options'::text))))
     LEFT JOIN recipe_lines rl ON (((rl.owner_type = 'modifier_option'::text) AND (rl.owner_id = mo.id) AND (rl.size_label IS NULL))))
     LEFT JOIN org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE (mo.legacy_source = 'optional'::text)$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.menu_item_optional_fields TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.menu_item_optional_fields TO sufrix;
        END IF;
    END IF;
END
$v$;

-- menu_item_recipes
DO $v$
BEGIN
    IF to_regclass('public.menu_item_recipes') IS NULL THEN
        EXECUTE $def$CREATE VIEW public.menu_item_recipes WITH (security_invoker = true) AS
 SELECT rl.id,
    ms.menu_item_id,
    ms.label AS size_label,
    rl.quantity AS quantity_used,
    rl.created_at,
    rl.updated_at,
    oi.name AS ingredient_name,
    rl.unit AS ingredient_unit,
    rl.ingredient_id AS org_ingredient_id
   FROM ((recipe_lines rl
     JOIN menu_item_sizes ms ON ((ms.id = rl.owner_id)))
     JOIN org_ingredients oi ON ((oi.id = rl.ingredient_id)))
  WHERE (rl.owner_type = 'item_size'::text)$def$;
        GRANT SELECT, INSERT, UPDATE, DELETE ON public.menu_item_recipes TO madar_app;
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
            GRANT SELECT ON public.menu_item_recipes TO sufrix;
        END IF;
    END IF;
END
$v$;
