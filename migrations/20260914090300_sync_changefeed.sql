-- One per-branch changefeed for everything the POS syncs (decision 16,
-- TILLS_CONTRACT.md §10.1). `POST /sync/pull?since=<seq>` reads it.
--
-- The feed is COMPACTED: one row per (branch, type, entity); a new change moves
-- the row's seq forward. The row carries no data — the pull projects the entity
-- at read time — so the feed is derived and can be dropped and rebuilt.
--
-- SOURCE TABLES (machine-read by the migration tests; keep in sync with
-- sync_source_tables() below). Format:  table -> type[, type…]  (scope)
--   categories                 -> category                    (org fan-out)
--   menu_items                 -> menu_item                   (org fan-out)
--   menu_item_sizes            -> menu_item (parent)          (org fan-out)
--   menu_item_modifier_groups  -> menu_item (parent)          (org fan-out)
--   modifier_groups            -> menu_item (every linked item) (org fan-out)
--   modifier_options           -> menu_item (every item linking the group) (org fan-out)
--   recipe_lines               -> menu_item (owner size / option's items) (org fan-out)
--   menu_price_overrides       -> menu_item (branch scope: that branch; channel scope: org fan-out)
--   menu_item_recipe_steps     -> menu_item (parent)          (org fan-out)
--   recipe_step_presets        -> menu_item (every item using the preset slug, all orgs)
--   menu_item_station_routes   -> menu_item (that branch)
--   category_station_routes    -> menu_item (items of the category, that branch)
--   bundles                    -> bundle                      (org fan-out)
--   bundle_components          -> bundle (parent)             (org fan-out)
--   bundle_branch_availability -> bundle (that branch)
--   org_ingredients            -> ingredient                  (org fan-out)
--   org_payment_methods        -> payment_method              (org fan-out)
--   branch_payment_methods     -> payment_availability (entity = branch id; that branch)
--   user_payment_methods       -> payment_availability (entity = user id; org fan-out)
--   device_payment_methods     -> payment_availability (entity = device id; org fan-out)
--   discounts                  -> discount                    (org fan-out)
--   branches                   -> branch_settings (entity = branch id; that branch)
--   kitchen_stations           -> branch_settings (that branch)
--   devices                    -> device                      (branch)
--   users                      -> teller                      (org fan-out)
--   user_branch_assignments    -> teller (parent)             (org fan-out)
--   permissions                -> teller (parent)             (org fan-out)
--   floor_sections             -> floor_section               (branch)
--   branch_tables              -> floor_table                 (branch)
--   table_occupancies          -> table_occupancy, floor_table (projected status) (branch)
--   booking_tables             -> booking, floor_table (next booking) (branch)
--   bookings                   -> booking, floor_table (via booking_tables) (branch)
--   table_transfer_requests    -> table_transfer              (branch)
--   open_tickets               -> open_ticket                 (branch)
--   open_ticket_items          -> open_ticket (parent)        (branch)
--   open_ticket_rounds         -> open_ticket (parent)        (branch)
--   kitchen_tickets            -> kitchen_ticket              (branch)
--   kitchen_ticket_items       -> kitchen_ticket (parent)     (branch)
--   delivery_orders            -> delivery                    (branch)
--   tills                      -> till                        (branch, LEDGER)
--   till_reconciliations       -> till (parent)               (branch, LEDGER)
--   till_cash_movements        -> cash_movement, till (aggregate) (branch, LEDGER)
--   orders                     -> order                       (branch, LEDGER)
--   order_items                -> order (parent)              (branch, LEDGER)
--   order_payments             -> order (parent)              (branch, LEDGER)
--   order_refunds              -> refund                      (branch, LEDGER)
--   order_refund_lines         -> refund (parent)             (branch, LEDGER)
--
-- Live sets (`op` = delete when a row leaves its set) are defined ONCE, by the
-- sync_live_<type>(row) predicates, and used by both the emitters and
-- sync_live_rows() (backfill, invariants, the B2 sweeper).
--
-- Emitters run SECURITY DEFINER: the tenant pool (madar_app, RLS) writes rows of
-- its own org, but the feed row it causes is bookkeeping, not tenant data, and
-- an org fan-out must see every branch of the org.

-- ── Feed tables ───────────────────────────────────────────────────────────────
CREATE SEQUENCE sync_changes_seq AS bigint;

CREATE TABLE sync_changes (
    branch_id   uuid        NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    type        text        NOT NULL,
    entity_id   uuid        NOT NULL,
    seq         bigint      NOT NULL DEFAULT nextval('sync_changes_seq'),
    op          text        NOT NULL CHECK (op IN ('upsert','delete')),
    xid         xid8        NOT NULL DEFAULT pg_current_xact_id(),
    changed_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, type, entity_id)
);
ALTER SEQUENCE sync_changes_seq OWNED BY sync_changes.seq;
CREATE UNIQUE INDEX uq_sync_changes_seq ON sync_changes (seq);
CREATE INDEX idx_sync_changes_branch_seq ON sync_changes (branch_id, seq);
CREATE INDEX idx_sync_changes_branch_type_op ON sync_changes (branch_id, type) WHERE op = 'upsert';

CREATE TABLE sync_feed_watermarks (
    branch_id          uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    purged_through_seq bigint NOT NULL DEFAULT 0,
    updated_at         timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE sync_changes ENABLE ROW LEVEL SECURITY;
ALTER TABLE sync_feed_watermarks ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON sync_changes FOR ALL
    USING (EXISTS (SELECT 1 FROM branches b WHERE b.id = sync_changes.branch_id
                     AND b.org_id = (SELECT current_setting('app.org_id', true)::uuid)));
CREATE POLICY tenant_isolation ON sync_feed_watermarks FOR ALL
    USING (EXISTS (SELECT 1 FROM branches b WHERE b.id = sync_feed_watermarks.branch_id
                     AND b.org_id = (SELECT current_setting('app.org_id', true)::uuid)));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE sync_changes, sync_feed_watermarks TO madar_app;
GRANT USAGE, SELECT ON SEQUENCE sync_changes_seq TO madar_app;

-- ── Core emit ────────────────────────────────────────────────────────────────
-- The per-branch advisory key. Emitters hold it SHARED until commit;
-- sync_safe_horizon takes it EXCLUSIVE (try-only) to find a seq below which no
-- uncommitted change can still appear.
CREATE FUNCTION sync_branch_lock_key(p_branch uuid) RETURNS bigint
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT hashtextextended('sync_changes:' || p_branch::text, 0) $$;

CREATE FUNCTION sync_emit(p_branch uuid, p_type text, p_id uuid, p_op text) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
BEGIN
    IF p_branch IS NULL OR p_id IS NULL THEN
        RETURN;
    END IF;
    PERFORM pg_advisory_xact_lock_shared(sync_branch_lock_key(p_branch));
    -- A branch being deleted cascades its rows away; its children's emits are moot.
    INSERT INTO sync_changes (branch_id, type, entity_id, op)
    SELECT p_branch, p_type, p_id, p_op
     WHERE EXISTS (SELECT 1 FROM branches WHERE id = p_branch)
    ON CONFLICT (branch_id, type, entity_id) DO UPDATE
       SET seq = nextval('sync_changes_seq'), op = EXCLUDED.op,
           xid = pg_current_xact_id(), changed_at = now();
    IF FOUND THEN
        PERFORM pg_notify('sync_changes', p_branch::text);
    END IF;
END;
$$;

-- Fans out to every non-deleted branch of the organisation.
CREATE FUNCTION sync_emit_org(p_org uuid, p_type text, p_id uuid, p_op text) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    b uuid;
BEGIN
    IF p_org IS NULL THEN
        RETURN;
    END IF;
    FOR b IN SELECT id FROM branches WHERE org_id = p_org AND deleted_at IS NULL ORDER BY id LOOP
        PERFORM sync_emit(b, p_type, p_id, p_op);
    END LOOP;
END;
$$;

CREATE FUNCTION sync_op(p_live boolean) RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT CASE WHEN p_live THEN 'upsert' ELSE 'delete' END $$;

-- Safe horizon (R-horizon support): the highest seq such that every change with
-- seq <= it for this branch is committed. Call it in READ COMMITTED (its second
-- statement needs a fresh snapshot), BEFORE a REPEATABLE READ full snapshot.
-- Never blocks writers: it only TRIES the exclusive key (writers hold it shared
-- until commit). When it cannot get the key in ~p_wait_ms it returns p_since
-- (no progress; the caller answers has_more=false and the device pulls again on
-- the next sync.changed / poll).
CREATE FUNCTION sync_safe_horizon(p_branch uuid, p_since bigint, p_wait_ms integer DEFAULT 200) RETURNS bigint
    LANGUAGE plpgsql VOLATILE
    AS $$
DECLARE
    k        bigint := sync_branch_lock_key(p_branch);
    h        bigint;
    deadline timestamptz := clock_timestamp() + make_interval(secs => p_wait_ms / 1000.0);
BEGIN
    LOOP
        EXIT WHEN pg_try_advisory_lock(k);
        IF clock_timestamp() >= deadline THEN
            RETURN p_since;
        END IF;
        PERFORM pg_sleep(0.005);
    END LOOP;
    -- New statement = new snapshot (READ COMMITTED): every emitter that held the
    -- key has committed or rolled back, so everything <= max(seq) is visible.
    SELECT max(seq) INTO h FROM sync_changes WHERE branch_id = p_branch;
    PERFORM pg_advisory_unlock(k);
    RETURN GREATEST(COALESCE(h, p_since), p_since);
END;
$$;

-- ── Live-set predicates (one per state type; ledger types are always live) ──
CREATE FUNCTION sync_live_category(r categories) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active AND r.deleted_at IS NULL $$;
CREATE FUNCTION sync_live_menu_item(r menu_items) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active AND r.deleted_at IS NULL $$;
CREATE FUNCTION sync_live_bundle(r bundles) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.status = 'active' $$;
CREATE FUNCTION sync_live_ingredient(r org_ingredients) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active AND r.deleted_at IS NULL $$;
CREATE FUNCTION sync_live_discount(r discounts) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active $$;
CREATE FUNCTION sync_live_branch_settings(r branches) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.deleted_at IS NULL $$;
CREATE FUNCTION sync_live_device(r devices) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.retired_at IS NULL $$;
-- Anyone who can sign in on a POS/KDS device (the session bundle): active,
-- not deleted, not a platform super admin, not a guest principal.
CREATE FUNCTION sync_live_teller(r users) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active AND r.deleted_at IS NULL AND r.role <> 'super_admin' AND NOT r.is_guest_principal $$;
CREATE FUNCTION sync_live_floor_table(r branch_tables) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active $$;
CREATE FUNCTION sync_live_table_occupancy(r table_occupancies) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.ended_at IS NULL OR (r.needs_bussing AND r.cleared_at IS NULL) $$;
CREATE FUNCTION sync_live_table_transfer(r table_transfer_requests) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.status = 'waiting' $$;
CREATE FUNCTION sync_live_open_ticket(r open_tickets) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.status = 'open' $$;
-- Time-based (the B2 sweeper re-evaluates these and emits the delete on age-out).
CREATE FUNCTION sync_live_kitchen_ticket(r kitchen_tickets) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.close_reason IS DISTINCT FROM 'retired'
                 AND (r.closed_at IS NULL OR r.closed_at >= now() - interval '12 hours') $$;
CREATE FUNCTION sync_live_delivery(r delivery_orders) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.status NOT IN ('delivered','cancelled','rejected')
                 OR r.updated_at >= now() - interval '48 hours' $$;
CREATE FUNCTION sync_live_booking(r bookings) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.status IN ('confirmed','seated') AND r.ends_at >= now() - interval '1 day' $$;

-- ── Parent re-emitters ───────────────────────────────────────────────────────
CREATE FUNCTION sync_touch_menu_item(p_item uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r menu_items;
BEGIN
    SELECT * INTO r FROM menu_items WHERE id = p_item;
    IF NOT FOUND THEN RETURN; END IF;
    IF p_branch IS NULL THEN
        PERFORM sync_emit_org(r.org_id, 'menu_item', r.id, sync_op(sync_live_menu_item(r)));
    ELSIF EXISTS (SELECT 1 FROM branches WHERE id = p_branch AND org_id = r.org_id AND deleted_at IS NULL) THEN
        PERFORM sync_emit(p_branch, 'menu_item', r.id, sync_op(sync_live_menu_item(r)));
    END IF;
END;
$$;

CREATE FUNCTION sync_touch_menu_items_of_group(p_group uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    i uuid;
BEGIN
    FOR i IN SELECT DISTINCT menu_item_id FROM menu_item_modifier_groups WHERE group_id = p_group ORDER BY 1 LOOP
        PERFORM sync_touch_menu_item(i, p_branch);
    END LOOP;
END;
$$;

CREATE FUNCTION sync_touch_bundle(p_bundle uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r bundles;
BEGIN
    SELECT * INTO r FROM bundles WHERE id = p_bundle;
    IF NOT FOUND THEN RETURN; END IF;
    IF p_branch IS NULL THEN
        PERFORM sync_emit_org(r.org_id, 'bundle', r.id, sync_op(sync_live_bundle(r)));
    ELSE
        PERFORM sync_emit(p_branch, 'bundle', r.id, sync_op(sync_live_bundle(r)));
    END IF;
END;
$$;

CREATE FUNCTION sync_touch_teller(p_user uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r users;
BEGIN
    SELECT * INTO r FROM users WHERE id = p_user;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM sync_emit_org(r.org_id, 'teller', r.id, sync_op(sync_live_teller(r)));
END;
$$;

CREATE FUNCTION sync_touch_floor_table(p_table uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r branch_tables;
BEGIN
    SELECT * INTO r FROM branch_tables WHERE id = p_table;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM sync_emit(r.branch_id, 'floor_table', r.id, sync_op(sync_live_floor_table(r)));
END;
$$;

CREATE FUNCTION sync_touch_booking(p_booking uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r bookings;
BEGIN
    SELECT * INTO r FROM bookings WHERE id = p_booking;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM sync_emit(r.branch_id, 'booking', r.id, sync_op(sync_live_booking(r)));
END;
$$;

CREATE FUNCTION sync_touch_open_ticket(p_ticket uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r open_tickets;
BEGIN
    SELECT * INTO r FROM open_tickets WHERE id = p_ticket;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM sync_emit(r.branch_id, 'open_ticket', r.id, sync_op(sync_live_open_ticket(r)));
END;
$$;

CREATE FUNCTION sync_touch_kitchen_ticket(p_ticket uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r kitchen_tickets;
BEGIN
    SELECT * INTO r FROM kitchen_tickets WHERE id = p_ticket;
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM sync_emit(r.branch_id, 'kitchen_ticket', r.id, sync_op(sync_live_kitchen_ticket(r)));
END;
$$;

CREATE FUNCTION sync_touch_till(p_till uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    b uuid;
BEGIN
    SELECT branch_id INTO b FROM tills WHERE id = p_till;
    PERFORM sync_emit(b, 'till', p_till, 'upsert');
END;
$$;

CREATE FUNCTION sync_touch_order(p_order uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    b uuid;
BEGIN
    SELECT branch_id INTO b FROM orders WHERE id = p_order;
    PERFORM sync_emit(b, 'order', p_order, 'upsert');
END;
$$;

CREATE FUNCTION sync_touch_refund(p_refund uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    b uuid;
BEGIN
    SELECT branch_id INTO b FROM order_refunds WHERE id = p_refund;
    PERFORM sync_emit(b, 'refund', p_refund, 'upsert');
END;
$$;

-- payment_availability: entity = owner id; upsert while the owner has >= 1 row.
CREATE FUNCTION sync_touch_payment_availability(p_scope text, p_owner uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    has_rows boolean;
    o        uuid;
BEGIN
    CASE p_scope
        WHEN 'branch' THEN
            has_rows := EXISTS (SELECT 1 FROM branch_payment_methods WHERE branch_id = p_owner);
            PERFORM sync_emit(p_owner, 'payment_availability', p_owner, sync_op(has_rows));
        WHEN 'user' THEN
            has_rows := EXISTS (SELECT 1 FROM user_payment_methods WHERE user_id = p_owner);
            SELECT org_id INTO o FROM users WHERE id = p_owner;
            PERFORM sync_emit_org(o, 'payment_availability', p_owner, sync_op(has_rows));
        WHEN 'device' THEN
            has_rows := EXISTS (SELECT 1 FROM device_payment_methods WHERE device_id = p_owner);
            SELECT org_id INTO o FROM devices WHERE id = p_owner;
            PERFORM sync_emit_org(o, 'payment_availability', p_owner, sync_op(has_rows));
    END CASE;
END;
$$;

-- ── Emitter trigger functions (one per source table) ─────────────────────────
-- Catalog
CREATE FUNCTION sync_emit_categories() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE i uuid; b uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'category', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'category', NEW.id, sync_op(sync_live_category(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_menu_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'menu_item', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'menu_item', NEW.id, sync_op(sync_live_menu_item(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_menu_item_sizes() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.menu_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.menu_item_id IS DISTINCT FROM OLD.menu_item_id) THEN
        PERFORM sync_touch_menu_item(NEW.menu_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_menu_item_modifier_groups() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.menu_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.menu_item_id IS DISTINCT FROM OLD.menu_item_id) THEN
        PERFORM sync_touch_menu_item(NEW.menu_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_modifier_groups() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    -- On DELETE the link rows cascade (their own trigger re-emits the items).
    IF TG_OP <> 'DELETE' THEN PERFORM sync_touch_menu_items_of_group(NEW.id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_modifier_options() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_items_of_group(OLD.group_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.group_id IS DISTINCT FROM OLD.group_id) THEN
        PERFORM sync_touch_menu_items_of_group(NEW.group_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_touch_recipe_owner(p_owner_type text, p_owner uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    x uuid;
BEGIN
    IF p_owner_type = 'item_size' THEN
        SELECT menu_item_id INTO x FROM menu_item_sizes WHERE id = p_owner;
        IF x IS NOT NULL THEN PERFORM sync_touch_menu_item(x); END IF;
    ELSIF p_owner_type = 'modifier_option' THEN
        SELECT group_id INTO x FROM modifier_options WHERE id = p_owner;
        IF x IS NOT NULL THEN PERFORM sync_touch_menu_items_of_group(x); END IF;
    END IF;
END;
$$;

CREATE FUNCTION sync_emit_recipe_lines() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_recipe_owner(OLD.owner_type, OLD.owner_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR (NEW.owner_type, NEW.owner_id) IS DISTINCT FROM (OLD.owner_type, OLD.owner_id)) THEN
        PERFORM sync_touch_recipe_owner(NEW.owner_type, NEW.owner_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_touch_price_override(p_target_type text, p_target uuid, p_branch uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    x uuid;
BEGIN
    IF p_target_type = 'menu_item_size' THEN
        SELECT menu_item_id INTO x FROM menu_item_sizes WHERE id = p_target;
        IF x IS NOT NULL THEN PERFORM sync_touch_menu_item(x, p_branch); END IF;
    ELSIF p_target_type = 'modifier_option' THEN
        SELECT group_id INTO x FROM modifier_options WHERE id = p_target;
        IF x IS NOT NULL THEN PERFORM sync_touch_menu_items_of_group(x, p_branch); END IF;
    END IF;
END;
$$;

CREATE FUNCTION sync_emit_menu_price_overrides() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    -- branch / branch_channel scope: that branch only; channel scope (branch_id NULL): org fan-out.
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_price_override(OLD.target_type, OLD.target_id, OLD.branch_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        PERFORM sync_touch_price_override(NEW.target_type, NEW.target_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_menu_item_recipe_steps() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.menu_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.menu_item_id IS DISTINCT FROM OLD.menu_item_id) THEN
        PERFORM sync_touch_menu_item(NEW.menu_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_recipe_step_presets() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    i uuid;
    s text := CASE WHEN TG_OP = 'DELETE' THEN OLD.slug ELSE NEW.slug END;
BEGIN
    FOR i IN SELECT DISTINCT menu_item_id FROM menu_item_recipe_steps WHERE preset_slug = s ORDER BY 1 LOOP
        PERFORM sync_touch_menu_item(i);
    END LOOP;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_menu_item_station_routes() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.menu_item_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_menu_item(NEW.menu_item_id, NEW.branch_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_touch_category_items(p_category uuid, p_branch uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    i uuid;
BEGIN
    FOR i IN SELECT id FROM menu_items WHERE category_id = p_category ORDER BY id LOOP
        PERFORM sync_touch_menu_item(i, p_branch);
    END LOOP;
END;
$$;

CREATE FUNCTION sync_emit_category_station_routes() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_category_items(OLD.category_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_category_items(NEW.category_id, NEW.branch_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_bundles() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'bundle', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'bundle', NEW.id, sync_op(sync_live_bundle(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_bundle_components() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_bundle(OLD.bundle_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.bundle_id IS DISTINCT FROM OLD.bundle_id) THEN
        PERFORM sync_touch_bundle(NEW.bundle_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_bundle_branch_availability() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_bundle(OLD.bundle_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_bundle(NEW.bundle_id, NEW.branch_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_org_ingredients() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'ingredient', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'ingredient', NEW.id, sync_op(sync_live_ingredient(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_org_payment_methods() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'payment_method', OLD.id, 'delete');
    ELSE
        -- Always upsert: an inactive method ships with is_active:false.
        PERFORM sync_emit_org(NEW.org_id, 'payment_method', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_branch_payment_methods() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_payment_availability('branch', OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_payment_availability('branch', NEW.branch_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_user_payment_methods() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_payment_availability('user', OLD.user_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_payment_availability('user', NEW.user_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_device_payment_methods() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_payment_availability('device', OLD.device_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_payment_availability('device', NEW.device_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_discounts() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'discount', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'discount', NEW.id, sync_op(sync_live_discount(NEW)));
    END IF;
    RETURN NULL;
END $$;

-- Branch-scoped settings / devices / people
CREATE FUNCTION sync_emit_branches() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    -- A hard DELETE cascades the branch's feed away; nothing to emit.
    IF TG_OP <> 'DELETE' THEN
        PERFORM sync_emit(NEW.id, 'branch_settings', NEW.id, sync_op(sync_live_branch_settings(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_kitchen_stations() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r branches;
    b uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.branch_id ELSE NEW.branch_id END;
BEGIN
    SELECT * INTO r FROM branches WHERE id = b;
    IF FOUND THEN
        PERFORM sync_emit(r.id, 'branch_settings', r.id, sync_op(sync_live_branch_settings(r)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_devices() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'device', OLD.id, 'delete');
        RETURN NULL;
    END IF;
    IF TG_OP = 'UPDATE' AND OLD.branch_id IS DISTINCT FROM NEW.branch_id THEN
        PERFORM sync_emit(OLD.branch_id, 'device', OLD.id, 'delete');
    END IF;
    PERFORM sync_emit(NEW.branch_id, 'device', NEW.id, sync_op(sync_live_device(NEW)));
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_users() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'teller', OLD.id, 'delete');
        RETURN NULL;
    END IF;
    IF TG_OP = 'UPDATE' AND OLD.org_id IS DISTINCT FROM NEW.org_id THEN
        PERFORM sync_emit_org(OLD.org_id, 'teller', OLD.id, 'delete');
    END IF;
    -- last_login_at alone is not a POS-visible change.
    IF TG_OP = 'UPDATE' AND (to_jsonb(NEW) - 'last_login_at' - 'updated_at') = (to_jsonb(OLD) - 'last_login_at' - 'updated_at') THEN
        RETURN NULL;
    END IF;
    PERFORM sync_emit_org(NEW.org_id, 'teller', NEW.id, sync_op(sync_live_teller(NEW)));
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_user_branch_assignments() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_teller(OLD.user_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.user_id IS DISTINCT FROM OLD.user_id) THEN
        PERFORM sync_touch_teller(NEW.user_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_permissions() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_teller(OLD.user_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.user_id IS DISTINCT FROM OLD.user_id) THEN
        PERFORM sync_touch_teller(NEW.user_id);
    END IF;
    RETURN NULL;
END $$;

-- Floor
CREATE FUNCTION sync_emit_floor_sections() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'floor_section', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'floor_section', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_branch_tables() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'floor_table', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'floor_table', NEW.id, sync_op(sync_live_floor_table(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_table_occupancies() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'table_occupancy', OLD.id, 'delete');
        PERFORM sync_touch_floor_table(OLD.table_id);
        RETURN NULL;
    END IF;
    PERFORM sync_emit(NEW.branch_id, 'table_occupancy', NEW.id, sync_op(sync_live_table_occupancy(NEW)));
    PERFORM sync_touch_floor_table(NEW.table_id);
    IF TG_OP = 'UPDATE' AND OLD.table_id IS DISTINCT FROM NEW.table_id THEN
        PERFORM sync_touch_floor_table(OLD.table_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_booking_tables() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_booking(OLD.booking_id);
        PERFORM sync_touch_floor_table(OLD.table_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        PERFORM sync_touch_booking(NEW.booking_id);
        PERFORM sync_touch_floor_table(NEW.table_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_bookings() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    t uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'booking', OLD.id, 'delete');
        RETURN NULL;
    END IF;
    PERFORM sync_emit(NEW.branch_id, 'booking', NEW.id, sync_op(sync_live_booking(NEW)));
    FOR t IN SELECT table_id FROM booking_tables WHERE booking_id = NEW.id ORDER BY 1 LOOP
        PERFORM sync_touch_floor_table(t);
    END LOOP;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_table_transfer_requests() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'table_transfer', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'table_transfer', NEW.id, sync_op(sync_live_table_transfer(NEW)));
    END IF;
    RETURN NULL;
END $$;

-- Bills + kitchen + delivery
CREATE FUNCTION sync_emit_open_tickets() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'open_ticket', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'open_ticket', NEW.id, sync_op(sync_live_open_ticket(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_open_ticket_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_open_ticket(OLD.open_ticket_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.open_ticket_id IS DISTINCT FROM OLD.open_ticket_id) THEN
        PERFORM sync_touch_open_ticket(NEW.open_ticket_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_open_ticket_rounds() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_open_ticket(OLD.open_ticket_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.open_ticket_id IS DISTINCT FROM OLD.open_ticket_id) THEN
        PERFORM sync_touch_open_ticket(NEW.open_ticket_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_kitchen_tickets() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'kitchen_ticket', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'kitchen_ticket', NEW.id, sync_op(sync_live_kitchen_ticket(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_kitchen_ticket_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_kitchen_ticket(OLD.kitchen_ticket_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.kitchen_ticket_id IS DISTINCT FROM OLD.kitchen_ticket_id) THEN
        PERFORM sync_touch_kitchen_ticket(NEW.kitchen_ticket_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_delivery_orders() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'delivery', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'delivery', NEW.id, sync_op(sync_live_delivery(NEW)));
    END IF;
    RETURN NULL;
END $$;

-- Ledger
CREATE FUNCTION sync_emit_tills() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'till', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'till', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_till_reconciliations() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_till(OLD.till_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.till_id IS DISTINCT FROM OLD.till_id) THEN
        PERFORM sync_touch_till(NEW.till_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_till_cash_movements() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    b uuid;
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        SELECT branch_id INTO b FROM tills WHERE id = OLD.till_id;
        IF TG_OP = 'DELETE' THEN
            PERFORM sync_emit(b, 'cash_movement', OLD.id, 'delete');
        END IF;
        PERFORM sync_touch_till(OLD.till_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        SELECT branch_id INTO b FROM tills WHERE id = NEW.till_id;
        PERFORM sync_emit(b, 'cash_movement', NEW.id, 'upsert');
        IF TG_OP = 'INSERT' OR NEW.till_id IS DISTINCT FROM OLD.till_id THEN
            PERFORM sync_touch_till(NEW.till_id);
        END IF;
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_orders() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'order', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'order', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_order(OLD.order_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.order_id IS DISTINCT FROM OLD.order_id) THEN
        PERFORM sync_touch_order(NEW.order_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_payments() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_order(OLD.order_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.order_id IS DISTINCT FROM OLD.order_id) THEN
        PERFORM sync_touch_order(NEW.order_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_refunds() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'refund', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit(NEW.branch_id, 'refund', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_refund_lines() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_refund(OLD.refund_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.refund_id IS DISTINCT FROM OLD.refund_id) THEN
        PERFORM sync_touch_refund(NEW.refund_id);
    END IF;
    RETURN NULL;
END $$;

-- ── Source registry + triggers ───────────────────────────────────────────────
CREATE FUNCTION sync_source_tables() RETURNS TABLE (source_table text, types text[])
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES
        ('categories',                 ARRAY['category']),
        ('menu_items',                 ARRAY['menu_item']),
        ('menu_item_sizes',            ARRAY['menu_item']),
        ('menu_item_modifier_groups',  ARRAY['menu_item']),
        ('modifier_groups',            ARRAY['menu_item']),
        ('modifier_options',           ARRAY['menu_item']),
        ('recipe_lines',               ARRAY['menu_item']),
        ('menu_price_overrides',       ARRAY['menu_item']),
        ('menu_item_recipe_steps',     ARRAY['menu_item']),
        ('recipe_step_presets',        ARRAY['menu_item']),
        ('menu_item_station_routes',   ARRAY['menu_item']),
        ('category_station_routes',    ARRAY['menu_item']),
        ('bundles',                    ARRAY['bundle']),
        ('bundle_components',          ARRAY['bundle']),
        ('bundle_branch_availability', ARRAY['bundle']),
        ('org_ingredients',            ARRAY['ingredient']),
        ('org_payment_methods',        ARRAY['payment_method']),
        ('branch_payment_methods',     ARRAY['payment_availability']),
        ('user_payment_methods',       ARRAY['payment_availability']),
        ('device_payment_methods',     ARRAY['payment_availability']),
        ('discounts',                  ARRAY['discount']),
        ('branches',                   ARRAY['branch_settings']),
        ('kitchen_stations',           ARRAY['branch_settings']),
        ('devices',                    ARRAY['device']),
        ('users',                      ARRAY['teller']),
        ('user_branch_assignments',    ARRAY['teller']),
        ('permissions',                ARRAY['teller']),
        ('floor_sections',             ARRAY['floor_section']),
        ('branch_tables',              ARRAY['floor_table']),
        ('table_occupancies',          ARRAY['table_occupancy','floor_table']),
        ('booking_tables',             ARRAY['booking','floor_table']),
        ('bookings',                   ARRAY['booking','floor_table']),
        ('table_transfer_requests',    ARRAY['table_transfer']),
        ('open_tickets',               ARRAY['open_ticket']),
        ('open_ticket_items',          ARRAY['open_ticket']),
        ('open_ticket_rounds',         ARRAY['open_ticket']),
        ('kitchen_tickets',            ARRAY['kitchen_ticket']),
        ('kitchen_ticket_items',       ARRAY['kitchen_ticket']),
        ('delivery_orders',            ARRAY['delivery']),
        ('tills',                      ARRAY['till']),
        ('till_reconciliations',       ARRAY['till']),
        ('till_cash_movements',        ARRAY['cash_movement','till']),
        ('orders',                     ARRAY['order']),
        ('order_items',                ARRAY['order']),
        ('order_payments',             ARRAY['order']),
        ('order_refunds',              ARRAY['refund']),
        ('order_refund_lines',         ARRAY['refund'])
    $$;

-- The type catalogue, with which are LEDGER (never age out; excluded from checksums).
CREATE FUNCTION sync_types() RETURNS TABLE (type text, is_ledger boolean)
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES ('category', false), ('menu_item', false), ('bundle', false), ('ingredient', false),
           ('payment_method', false), ('payment_availability', false), ('discount', false),
           ('branch_settings', false), ('device', false), ('teller', false),
           ('floor_section', false), ('floor_table', false), ('table_occupancy', false),
           ('table_transfer', false), ('open_ticket', false), ('kitchen_ticket', false),
           ('delivery', false), ('booking', false),
           ('till', true), ('cash_movement', true), ('order', true), ('refund', true)
    $$;

DO $$
DECLARE
    t text;
BEGIN
    FOR t IN SELECT source_table FROM sync_source_tables() LOOP
        EXECUTE format(
            'CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION %I()',
            t, 'sync_emit_' || t);
    END LOOP;
END $$;

-- ── Live rows (backfill, invariants, sweeper) ─────────────────────────────────
-- Every (branch, type, entity) that should hold an `upsert` row right now.
CREATE FUNCTION sync_live_rows() RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL)
    SELECT ob.branch_id, 'category', x.id FROM categories x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_category(x)
    UNION ALL
    SELECT ob.branch_id, 'menu_item', x.id FROM menu_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_menu_item(x)
    UNION ALL
    SELECT ob.branch_id, 'bundle', x.id FROM bundles x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_bundle(x)
    UNION ALL
    SELECT ob.branch_id, 'ingredient', x.id FROM org_ingredients x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_ingredient(x)
    UNION ALL
    SELECT ob.branch_id, 'payment_method', x.id FROM org_payment_methods x JOIN ob ON ob.org_id = x.org_id
    UNION ALL
    SELECT DISTINCT x.branch_id, 'payment_availability', x.branch_id FROM branch_payment_methods x JOIN ob ON ob.branch_id = x.branch_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.user_id FROM user_payment_methods x JOIN users u ON u.id = x.user_id JOIN ob ON ob.org_id = u.org_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.device_id FROM device_payment_methods x JOIN devices d ON d.id = x.device_id JOIN ob ON ob.org_id = d.org_id
    UNION ALL
    SELECT ob.branch_id, 'discount', x.id FROM discounts x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_discount(x)
    UNION ALL
    SELECT x.id, 'branch_settings', x.id FROM branches x WHERE sync_live_branch_settings(x)
    UNION ALL
    SELECT x.branch_id, 'device', x.id FROM devices x WHERE x.branch_id IS NOT NULL AND sync_live_device(x)
    UNION ALL
    SELECT ob.branch_id, 'teller', x.id FROM users x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_teller(x)
    UNION ALL
    SELECT x.branch_id, 'floor_section', x.id FROM floor_sections x
    UNION ALL
    SELECT x.branch_id, 'floor_table', x.id FROM branch_tables x WHERE sync_live_floor_table(x)
    UNION ALL
    SELECT x.branch_id, 'table_occupancy', x.id FROM table_occupancies x WHERE sync_live_table_occupancy(x)
    UNION ALL
    SELECT x.branch_id, 'table_transfer', x.id FROM table_transfer_requests x WHERE sync_live_table_transfer(x)
    UNION ALL
    SELECT x.branch_id, 'open_ticket', x.id FROM open_tickets x WHERE sync_live_open_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'kitchen_ticket', x.id FROM kitchen_tickets x WHERE sync_live_kitchen_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'delivery', x.id FROM delivery_orders x WHERE sync_live_delivery(x)
    UNION ALL
    SELECT x.branch_id, 'booking', x.id FROM bookings x WHERE sync_live_booking(x)
    UNION ALL
    SELECT x.branch_id, 'till', x.id FROM tills x
    UNION ALL
    SELECT t.branch_id, 'cash_movement', x.id FROM till_cash_movements x JOIN tills t ON t.id = x.till_id
    UNION ALL
    SELECT x.branch_id, 'order', x.id FROM orders x
    UNION ALL
    SELECT x.branch_id, 'refund', x.id FROM order_refunds x
    $$;

-- ── Backfill: the feed is complete from day one ──────────────────────────────
-- Ordered so a branch's seqs follow a stable (type, id) order.
INSERT INTO sync_changes (branch_id, type, entity_id, op)
SELECT l.branch_id, l.type, l.entity_id, 'upsert'
  FROM sync_live_rows() l
 ORDER BY l.branch_id, l.type, l.entity_id
ON CONFLICT DO NOTHING;

INSERT INTO sync_feed_watermarks (branch_id, purged_through_seq)
SELECT id, 0 FROM branches;

DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM (
        (SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert'
         EXCEPT SELECT DISTINCT branch_id, type, entity_id FROM sync_live_rows())
        UNION ALL
        (SELECT DISTINCT branch_id, type, entity_id FROM sync_live_rows()
         EXCEPT SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert')
    ) d;
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant: changefeed backfill differs from live sets by % row(s)', bad;
    END IF;
END $$;
