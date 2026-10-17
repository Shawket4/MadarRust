-- DB architecture Phase 3.4 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 3.4), EXPAND:
-- `menu_item_modifier_groups.included_option_ids uuid[]` → a relational join
-- table `menu_item_modifier_group_options`.
--
-- The array means: NULL = every option of the group is offered; an array =
-- only these options, in this order (the order is the legacy allowlist's
-- `sort_order`). A join table alone cannot tell "no rows" (offer all) from "an
-- empty allowlist", so the attachment also gets `option_allowlist boolean`
-- (= included_option_ids IS NOT NULL).
--
-- Dual write is done HERE, by triggers, not by each writer: the studio, the
-- modifier routes and the menu-unification backfill all write the array, and
-- a trigger cannot miss one.
--   * BEFORE INSERT/UPDATE on the attachment: option_allowlist := array IS NOT NULL;
--   * AFTER INSERT/UPDATE OF included_option_ids: the attachment's join rows are
--     replaced by the array's elements (with their position), skipping an id
--     that names no option (none in prod; the join has a real FK).
-- Reads switch to the join table in the same release. The array column, the
-- triggers and the writers' array writes go in a later release:
-- migrations/parked/p3_mimg_option_allowlist_contract.sql.
--
-- Backfill: the 56 prod attachments (1,130 elements), in this transaction;
-- idempotent (ON CONFLICT DO NOTHING; flag only where it differs). The
-- attachment's own UPDATE triggers (changefeed, catalog revision) are off for
-- that one statement, as in Phase 1.2: the flag is derived, nothing changed.
-- Reconciliation: scripts/db_phase3_check.sql (per org: elements naming a live
-- option = join rows, and the set difference is empty both ways).
SET LOCAL lock_timeout = '15s';

CREATE UNIQUE INDEX IF NOT EXISTS uq_mimg_org_id_id ON menu_item_modifier_groups (org_id, id);

ALTER TABLE menu_item_modifier_groups
    ADD COLUMN IF NOT EXISTS option_allowlist boolean NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS menu_item_modifier_group_options (
    org_id     uuid    NOT NULL,
    mimg_id    uuid    NOT NULL,
    option_id  uuid    NOT NULL REFERENCES modifier_options (id) ON DELETE CASCADE,
    sort       integer NOT NULL,
    PRIMARY KEY (mimg_id, option_id),
    FOREIGN KEY (org_id, mimg_id) REFERENCES menu_item_modifier_groups (org_id, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_mimg_options_option ON menu_item_modifier_group_options (option_id);

-- The tenant trio (CLAUDE.md "Tenant children carry org_id").
CREATE OR REPLACE FUNCTION fill_org_id_menu_item_modifier_group_options() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.org_id IS NULL THEN
        SELECT m.org_id INTO NEW.org_id FROM menu_item_modifier_groups m WHERE m.id = NEW.mimg_id;
        IF NEW.org_id IS NULL THEN
            RAISE EXCEPTION 'menu_item_modifier_group_options: no organization for attachment %', NEW.mimg_id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS fill_org_id ON menu_item_modifier_group_options;
CREATE TRIGGER fill_org_id BEFORE INSERT ON menu_item_modifier_group_options
    FOR EACH ROW EXECUTE FUNCTION fill_org_id_menu_item_modifier_group_options();

ALTER TABLE menu_item_modifier_group_options ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON menu_item_modifier_group_options;
CREATE POLICY tenant_isolation ON menu_item_modifier_group_options FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON menu_item_modifier_group_options TO madar_app;

-- ── dual write ──────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION mimg_option_allowlist_flag() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.option_allowlist := NEW.included_option_ids IS NOT NULL;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS mimg_option_allowlist_flag ON menu_item_modifier_groups;
CREATE TRIGGER mimg_option_allowlist_flag BEFORE INSERT OR UPDATE ON menu_item_modifier_groups
    FOR EACH ROW EXECUTE FUNCTION mimg_option_allowlist_flag();

CREATE OR REPLACE FUNCTION mimg_option_allowlist_sync() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.included_option_ids IS NOT DISTINCT FROM OLD.included_option_ids THEN
        RETURN NULL;
    END IF;
    DELETE FROM menu_item_modifier_group_options WHERE mimg_id = NEW.id;
    INSERT INTO menu_item_modifier_group_options (org_id, mimg_id, option_id, sort)
    SELECT NEW.org_id, NEW.id, u.opt, (min(u.ord) - 1)::int
      FROM unnest(NEW.included_option_ids) WITH ORDINALITY AS u(opt, ord)
     WHERE EXISTS (SELECT 1 FROM modifier_options o WHERE o.id = u.opt)
     GROUP BY u.opt;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS mimg_option_allowlist_sync ON menu_item_modifier_groups;
CREATE TRIGGER mimg_option_allowlist_sync AFTER INSERT OR UPDATE OF included_option_ids
    ON menu_item_modifier_groups FOR EACH ROW EXECUTE FUNCTION mimg_option_allowlist_sync();

-- ── backfill (idempotent) ───────────────────────────────────────────────────
INSERT INTO menu_item_modifier_group_options (org_id, mimg_id, option_id, sort)
SELECT m.org_id, m.id, u.opt, (min(u.ord) - 1)::int
  FROM menu_item_modifier_groups m
 CROSS JOIN LATERAL unnest(m.included_option_ids) WITH ORDINALITY AS u(opt, ord)
 WHERE EXISTS (SELECT 1 FROM modifier_options o WHERE o.id = u.opt)
 GROUP BY m.org_id, m.id, u.opt
ON CONFLICT (mimg_id, option_id) DO NOTHING;

DO $flag$
DECLARE
    trg  record;
    off  text[] := '{}';
    mode text[] := '{}';
    i    int;
BEGIN
    FOR trg IN
        SELECT tgname, tgenabled FROM pg_trigger
         WHERE tgrelid = 'menu_item_modifier_groups'::regclass AND NOT tgisinternal
           AND tgenabled <> 'D' AND (tgtype & 16) <> 0
           AND tgname NOT IN ('mimg_option_allowlist_flag')
         ORDER BY tgname
    LOOP
        EXECUTE format('ALTER TABLE menu_item_modifier_groups DISABLE TRIGGER %I', trg.tgname);
        off  := off  || trg.tgname::text;
        mode := mode || trg.tgenabled::text;
    END LOOP;
    -- The BEFORE flag trigger sets it from the array.
    UPDATE menu_item_modifier_groups SET option_allowlist = (included_option_ids IS NOT NULL)
     WHERE option_allowlist IS DISTINCT FROM (included_option_ids IS NOT NULL);
    FOR i IN 1 .. coalesce(array_length(off, 1), 0) LOOP
        EXECUTE format('ALTER TABLE menu_item_modifier_groups ENABLE %s TRIGGER %I',
            CASE mode[i] WHEN 'A' THEN 'ALWAYS' WHEN 'R' THEN 'REPLICA' ELSE '' END, off[i]);
    END LOOP;
END
$flag$;

-- What a reader asks: the allowlist as the old array shape (NULL = all), from
-- the join table. STABLE, so a query calls it per row it returns.
CREATE OR REPLACE FUNCTION mimg_included_options(p_mimg uuid, p_allowlist boolean)
RETURNS uuid[] LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN p_allowlist THEN
        coalesce((SELECT array_agg(x.option_id ORDER BY x.sort, x.option_id)
                    FROM menu_item_modifier_group_options x WHERE x.mimg_id = p_mimg), '{}')
    END
$$;
